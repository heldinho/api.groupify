use crate::utils::internal_error;
use crate::{InnerState};

use std::sync::Arc;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose;
use base64::Engine;
use rand::Rng;
use sqlx::{FromRow, Row, SqlitePool};
use url::Url;

const DEFAULT_CACHE_CONTROL_HEADER_VALUE: &str =
    "public, max-age=300, s-maxage=300, state-while-revalidate=300, stale-if-error=300";

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Link {
    pub id: String,
    pub target_url: String,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkTarget {
    pub target_url: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CounterLinkStatistics {
    pub amount: Option<i64>,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
}


fn generate_id() -> String {
    let random_number = rand::thread_rng().gen_range(0..u32::MAX);
    general_purpose::URL_SAFE_NO_PAD.encode(random_number.to_string())
}

pub async fn redirect(
    State(inner): State<InnerState>,
    Path(requested_link): Path<String>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let InnerState { db, .. } = inner;

    let link = sqlx::query_as!(
        Link,
        r#"select id, target_url from links where id = $1"#,
        requested_link
    )
    .fetch_optional(&db)
    .await
    .map_err(internal_error)?
    .ok_or_else(|| "Not Found".to_string())
    .map_err(|err| (StatusCode::NOT_FOUND, err))?;

    tracing::debug!(
        "Redirecting link id {} to {}",
        requested_link,
        link.target_url
    );

    let referer_header = headers
        .get("referer")
        .map(|value| value.to_str().unwrap_or_default().to_string());

    let user_agent_header = headers
        .get("user-agent")
        .map(|value| value.to_str().unwrap_or_default().to_string());

    let insert_statistics_timeout = tokio::time::Duration::from_millis(1000);

    let saved_statistics = tokio::time::timeout(
        insert_statistics_timeout,
        sqlx::query_as!(
            CounterLinkStatistics,
            r#"
            insert into link_statistics(link_id, referer, user_agent)
            values($1, $2, $3)
            "#,
            requested_link,
            referer_header,
            user_agent_header
        )
        .execute(&db),
    )
    .await;

    match saved_statistics {
        Err(elapsed) => tracing::error!("Saving new link click resulted in a timeout: {}", elapsed),
        Ok(Err(err)) => {
            tracing::error!("Saving a new link failed with the following error: {}", err)
        }
        _ => tracing::debug!(
            "Persisted new link click for link with id {}, referer {}, and user_agent {}",
            requested_link,
            referer_header.unwrap_or_default(),
            user_agent_header.unwrap_or_default()
        ),
    };

    Ok(Response::builder()
        .status(StatusCode::TEMPORARY_REDIRECT)
        .header("Location", link.target_url)
        .header("Cache-Control", DEFAULT_CACHE_CONTROL_HEADER_VALUE)
        .body(Body::empty())
        .expect("This response should always be constructable"))
}

pub async fn create_link(
    State(inner): State<InnerState>,
    Json(new_link): Json<LinkTarget>,
) -> Result<Json<Link>, (StatusCode, String)> {
    let InnerState { db, .. } = inner;

    let url = Url::parse(&new_link.target_url)
        .map_err(|_| (StatusCode::CONFLICT, "url malformed".into()))?
        .to_string();

    let new_link_id = generate_id();
    let fetch_statistics_timeout = tokio::time::Duration::from_millis(1000);

    let new_link = tokio::time::timeout(
        fetch_statistics_timeout,
        sqlx::query_as!(
            Link,
            r#"
            INSERT INTO links (id, target_url) VALUES ($1, $2) RETURNING id, target_url
            "#,
            new_link_id,
            url,
        )
            .fetch_one(&db)
    )
        .await
        .map_err(internal_error)?
        .map_err(internal_error)?;

    tracing::debug!("Created new link with id {} targeting {}", new_link_id, url);

    Ok(Json(new_link))
}

pub async fn update_link(
    State(inner): State<InnerState>,
    Path(link_id): Path<String>,
    Json(update_link): Json<LinkTarget>,
) -> Result<Json<Link>, (StatusCode, String)> {
    let InnerState { db, .. } = inner;

    let url = Url::parse(&update_link.target_url)
        .map_err(|_| (StatusCode::CONFLICT, "Url malformed".into()))?
        .to_string();

    let fetch_statistics_timeout = tokio::time::Duration::from_millis(1000);

    let link = tokio::time::timeout(
        fetch_statistics_timeout,
        sqlx::query_as!(
            Link,
            r#"
            update links set target_url = $1 where id = $2 returning id, target_url
            "#,
            url,
            link_id,
        )
            .fetch_one(&db)
    )
        .await
        .map_err(internal_error)?
        .map_err(internal_error)?;

    tracing::debug!("Updated link with id {}, now targeting {}", link_id, url);

    Ok(Json(link))
}

pub async fn get_link_statistics(
    State(inner): State<InnerState>,
    Path(link_id): Path<String>,
) -> Result<Json<Vec<CounterLinkStatistics>>, (StatusCode, String)> {
    let InnerState { db, .. } = inner;

    let fetch_statistics_timeout = tokio::time::Duration::from_millis(1000);

    let statistics = tokio::time::timeout(
        fetch_statistics_timeout,
        sqlx::query_as!(
            CounterLinkStatistics,
            r#"
            select count(*) as amount, referer, user_agent from link_statistics group by link_id, referer, user_agent having link_id = $1
            "#,
            link_id
        )
            .fetch_all(&db)
    )
        .await
        .map_err(internal_error)?
        .map_err(internal_error)?;

    tracing::debug!("Statistics for link with id {} requested", link_id);

    Ok(Json(statistics))
}