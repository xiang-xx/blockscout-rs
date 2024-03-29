use crate::{
    entity::subgraph::domain::{DetailedDomain, Domain, DomainWithAddress, ReverseRecord},
    hash_name::hex,
    subgraphs_reader::{
        domain_name::DomainName, pagination::Paginator, GetDomainInput, LookupAddressInput,
        SubgraphReadError,
    },
};
use anyhow::Context;
use ethers::addressbook::Address;
use sea_query::{Alias, Condition, Expr, PostgresQueryBuilder, SelectStatement};
use sqlx::postgres::{PgPool, PgQueryResult};
use tracing::instrument;

mod sql_gen {
    use super::*;

    pub trait QueryBuilderExt {
        fn with_block_range(&mut self) -> &mut Self;

        fn with_non_empty_label(&mut self) -> &mut Self;

        fn with_not_expired(&mut self) -> &mut Self;

        fn with_resolved_names(&mut self) -> &mut Self;
    }

    impl QueryBuilderExt for sea_query::SelectStatement {
        fn with_block_range(&mut self) -> &mut SelectStatement {
            self.and_where(Expr::cust(DOMAIN_BLOCK_RANGE_WHERE_CLAUSE))
        }

        fn with_non_empty_label(&mut self) -> &mut SelectStatement {
            self.and_where(Expr::cust(DOMAIN_NONEMPTY_LABEL_WHERE_CLAUSE))
        }

        fn with_not_expired(&mut self) -> &mut SelectStatement {
            self.and_where(Expr::cust(DOMAIN_NOT_EXPIRED_WHERE_CLAUSE))
        }

        fn with_resolved_names(&mut self) -> &mut SelectStatement {
            self.and_where(Expr::cust("name NOT LIKE '%[%'"))
        }
    }

    #[allow(dead_code)]
    pub fn detailed_domain_select(schema: &str) -> SelectStatement {
        sea_query::Query::select()
            .expr(Expr::cust(DETAILED_DOMAIN_DEFAULT_SELECT_CLAUSE))
            .from((Alias::new(schema), Alias::new("domain")))
            .to_owned()
    }

    pub fn domain_select(schema: &str) -> SelectStatement {
        domain_select_custom(schema, DOMAIN_DEFAULT_SELECT_CLAUSE)
    }

    pub fn domain_select_custom(schema: &str, select: &str) -> SelectStatement {
        sea_query::Query::select()
            .expr(Expr::cust(select))
            .from((Alias::new(schema), Alias::new("domain")))
            .to_owned()
    }
}
use crate::subgraphs_reader::{sql::bind_string_list, DomainPaginationInput};
use sql_gen::QueryBuilderExt;

const DETAILED_DOMAIN_DEFAULT_SELECT_CLAUSE: &str = r#"
vid,
block_range,
id,
name,
label_name,
labelhash,
parent,
subdomain_count,
resolved_address,
resolver,
to_timestamp(ttl) as ttl,
is_migrated,
created_at,
to_timestamp(created_at) as registration_date,
owner,
registrant,
wrapped_owner,
to_timestamp(expiry_date) as expiry_date,
COALESCE(to_timestamp(expiry_date) < now(), false) AS is_expired
"#;

const DOMAIN_DEFAULT_SELECT_CLAUSE: &str = r#"
id,
name,
resolved_address,
created_at,
to_timestamp(created_at) as registration_date,
owner,
wrapped_owner,
to_timestamp(expiry_date) as expiry_date,
COALESCE(to_timestamp(expiry_date) < now(), false) AS is_expired
"#;

// `block_range @>` is special sql syntax for fast filtering int4range
// to access current version of domain.
// Source: https://github.com/graphprotocol/graph-node/blob/19fd41bb48511f889dc94f5d82e16cd492f29da1/store/postgres/src/block_range.rs#L26
pub const DOMAIN_BLOCK_RANGE_WHERE_CLAUSE: &str = "block_range @> 2147483647";

pub const DOMAIN_NONEMPTY_LABEL_WHERE_CLAUSE: &str = "label_name IS NOT NULL";

pub const DOMAIN_NOT_EXPIRED_WHERE_CLAUSE: &str = r#"
(
    expiry_date is null
    OR to_timestamp(expiry_date) > now()
)
"#;

// TODO: rewrite to sea_query generation
#[instrument(name = "get_domain", skip(pool), err(level = "error"), level = "info")]
pub async fn get_domain(
    pool: &PgPool,
    domain_name: &DomainName,
    schema: &str,
    input: &GetDomainInput,
) -> Result<Option<DetailedDomain>, SubgraphReadError> {
    let only_active_clause = input
        .only_active
        .then(|| format!("AND {DOMAIN_NOT_EXPIRED_WHERE_CLAUSE}"))
        .unwrap_or_default();
    let maybe_domain = sqlx::query_as(&format!(
        r#"
        SELECT
            {DETAILED_DOMAIN_DEFAULT_SELECT_CLAUSE},
            COALESCE(
                multi_coin_addresses.coin_to_addr,
                '{{}}'::json
            ) as other_addresses
        FROM {schema}.domain
        LEFT JOIN (
            SELECT 
                d.id as domain_id, json_object_agg(mac.coin_type, encode(mac.addr, 'hex')) AS coin_to_addr 
            FROM {schema}.domain d
            LEFT JOIN {schema}.multicoin_addr_changed mac ON d.resolver = mac.resolver
            WHERE 
                d.id = $1
                AND d.{DOMAIN_BLOCK_RANGE_WHERE_CLAUSE}
                AND mac.coin_type IS NOT NULL
                AND mac.addr IS NOT NULL
            GROUP BY d.id
        ) multi_coin_addresses ON {schema}.domain.id = multi_coin_addresses.domain_id
        WHERE 
            id = $1 
            AND {DOMAIN_BLOCK_RANGE_WHERE_CLAUSE}
        {only_active_clause}
        ;"#,
    ))
    .bind(&domain_name.id)
    .fetch_optional(pool)
    .await?;
    Ok(maybe_domain)
}

#[instrument(
    name = "find_domains",
    skip(pool),
    err(level = "error"),
    level = "info"
)]
pub async fn find_domains(
    pool: &PgPool,
    schema: &str,
    domain_names: Option<Vec<&DomainName>>,
    only_active: bool,
    pagination: Option<&DomainPaginationInput>,
) -> Result<Vec<Domain>, SubgraphReadError> {
    let mut query = sql_gen::domain_select(schema);
    let mut q = query.with_block_range();
    if only_active {
        q = q.with_not_expired();
    };
    if domain_names.is_some() {
        q = q.and_where(Expr::cust("id = ANY($1)"));
    } else {
        q = q.with_non_empty_label().with_resolved_names();
    }

    if let Some(pagination) = pagination {
        pagination
            .add_to_query(q)
            .context("adding pagination to query")
            .map_err(|e| SubgraphReadError::Internal(e.to_string()))?;
    }

    let sql = q.to_string(PostgresQueryBuilder);
    let mut query = sqlx::query_as(&sql);
    tracing::debug!(sql = sql, "build SQL query for 'find_domains'");
    if let Some(domain_names) = domain_names {
        query = query.bind(
            domain_names
                .iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>(),
        );
    };
    let domains = query.fetch_all(pool).await?;
    Ok(domains)
}

#[instrument(
    name = "find_resolved_addresses",
    skip(pool),
    err(level = "error"),
    level = "info"
)]
pub async fn find_resolved_addresses(
    pool: &PgPool,
    schema: &str,
    input: &LookupAddressInput,
) -> Result<Vec<Domain>, SubgraphReadError> {
    let sql = gen_sql_select_domains_by_address(
        schema,
        None,
        input.only_active,
        input.resolved_to,
        input.owned_by,
        Some(&input.pagination),
    )?;

    let domains = sqlx::query_as(&sql)
        .bind(hex(input.address))
        .fetch_all(pool)
        .await?;
    Ok(domains)
}

#[instrument(
    name = "count_domains_by_address",
    skip(pool),
    err(level = "error"),
    level = "info"
)]
pub async fn count_domains_by_address(
    pool: &PgPool,
    schema: &str,
    address: Address,
    only_active: bool,
    resolved_to: bool,
    owned_by: bool,
) -> Result<i64, SubgraphReadError> {
    let sql = gen_sql_select_domains_by_address(
        schema,
        Some("COUNT(*)"),
        only_active,
        resolved_to,
        owned_by,
        None,
    )?;

    let count: i64 = sqlx::query_scalar(&sql)
        .bind(hex(address))
        .fetch_one(pool)
        .await?;
    Ok(count)
}

fn gen_sql_select_domains_by_address(
    schema: &str,
    select_clause: Option<&str>,
    only_active: bool,
    resolved_to: bool,
    owned_by: bool,
    pagination: Option<&DomainPaginationInput>,
) -> Result<String, SubgraphReadError> {
    let mut query = if let Some(select_clause) = select_clause {
        sql_gen::domain_select_custom(schema, select_clause)
    } else {
        sql_gen::domain_select(schema)
    };

    let mut q = query
        .with_block_range()
        .with_non_empty_label()
        .with_resolved_names();
    if only_active {
        q = q.with_not_expired();
    };

    // Trick: in resolved_to and owned_by are not provided, binding still exists and `cond` will be false
    let mut main_cond = Condition::any().add(Expr::cust("$1 <> $1"));
    if resolved_to {
        main_cond = main_cond.add(Expr::cust("resolved_address = $1"));
    }
    if owned_by {
        main_cond = main_cond.add(Expr::cust("owner = $1"));
        main_cond = main_cond.add(Expr::cust("wrapped_owner = $1"));
    }
    q = q.cond_where(main_cond);

    if let Some(pagination) = pagination {
        pagination
            .add_to_query(q)
            .context("adding pagination to query")
            .map_err(|e| SubgraphReadError::Internal(e.to_string()))?;
    }

    Ok(q.to_string(PostgresQueryBuilder))
}

// TODO: rewrite to sea_query generation
#[instrument(
    name = "batch_search_addresses",
    skip(pool, addresses),
    fields(job_size = addresses.len()),
    err(level = "error"),
    level = "info",
)]
pub async fn batch_search_addresses(
    pool: &PgPool,
    schema: &str,
    addresses: &[impl AsRef<str>],
) -> Result<Vec<DomainWithAddress>, SubgraphReadError> {
    let domains: Vec<DomainWithAddress> = sqlx::query_as(&format!(
        r#"
        SELECT DISTINCT ON (resolved_address) id, name AS domain_name, resolved_address
        FROM {schema}.domain
        WHERE
            resolved_address = ANY($1)
            AND name NOT LIKE '%[%'
            AND {DOMAIN_BLOCK_RANGE_WHERE_CLAUSE}
            AND {DOMAIN_NONEMPTY_LABEL_WHERE_CLAUSE}
            AND {DOMAIN_NOT_EXPIRED_WHERE_CLAUSE}
        ORDER BY resolved_address, created_at
        "#,
    ))
    .bind(bind_string_list(addresses))
    .fetch_all(pool)
    .await?;

    Ok(domains)
}

#[instrument(
    name = "batch_search_addr_reverse_names",
    skip(pool, addr_reverse_hashes),
    fields(job_size = addr_reverse_hashes.len()),
    err(level = "error"),
    level = "info",
)]
pub async fn batch_search_addr_reverse_names(
    pool: &PgPool,
    schema: &str,
    addr_reverse_hashes: &[impl AsRef<str>],
) -> Result<Vec<ReverseRecord>, SubgraphReadError> {
    let domains: Vec<ReverseRecord> = sqlx::query_as(&format!(
        r#"
        SELECT d.id as addr_reverse_id, nc.name as reversed_name
        FROM {schema}.domain d
        JOIN {schema}.name_changed nc ON nc.resolver = d.resolver
        WHERE d.id = ANY($1)
            AND d.{DOMAIN_BLOCK_RANGE_WHERE_CLAUSE}
        ORDER BY nc.block_number DESC;
        "#,
    ))
    .bind(bind_string_list(addr_reverse_hashes))
    .fetch_all(pool)
    .await?;

    Ok(domains)
}

// TODO: rewrite to sea_query generation
#[instrument(
    name = "update_domain_name",
    skip(pool),
    err(level = "error"),
    level = "info"
)]
pub async fn update_domain_name(
    pool: &PgPool,
    schema: &str,
    name: &DomainName,
) -> Result<PgQueryResult, sqlx::Error> {
    let result = sqlx::query(&format!(
        "UPDATE {schema}.domain SET name = $1, label_name = $2 WHERE id = $3;"
    ))
    .bind(&name.name)
    .bind(&name.label_name)
    .bind(&name.id)
    .execute(pool)
    .await?;
    Ok(result)
}
