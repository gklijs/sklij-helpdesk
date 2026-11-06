//! Company discovery for the browser. Discovery never grants access: every
//! entry requires an active mapping on the context that will serve its tickets.
use crate::{
    helpdesk::BOUNDED_CONTEXT, routing::RoutingMode, tenant_access::recorded_company_tenants,
};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::get,
    Json, Router,
};
use serde::Serialize;
use skilj_core::{
    access_control::{self, IdpConfig, JwksCache, Role},
    db,
};
use std::{collections::BTreeMap, sync::Arc};

pub struct DirectoryState {
    pub pool: db::Pool,
    pub idp: IdpConfig,
    pub keys: JwksCache,
    pub mode: RoutingMode,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanyAccess {
    pub company_id: String,
    pub bounded_context: String,
}

pub fn router(state: Arc<DirectoryState>) -> Router {
    Router::new()
        .route("/v1/helpdesk/companies", get(companies))
        .with_state(state)
}

async fn companies(
    State(state): State<Arc<DirectoryState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<CompanyAccess>>, (StatusCode, String)> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or((StatusCode::UNAUTHORIZED, "sign in again".into()))?;
    let subject = access_control::verify_and_extract_subject(token, &state.idp, &state.keys)
        .await
        .map_err(|_| (StatusCode::UNAUTHORIZED, "sign in again".into()))?;
    let roles = db::active_roles_by_external_subject(&state.pool, &subject)
        .await
        .map_err(internal)?;
    let role = roles
        .first()
        .ok_or((StatusCode::UNAUTHORIZED, "role is no longer active".into()))?;
    authorized_companies(&state.pool, role, state.mode)
        .await
        .map(Json)
        .map_err(internal)
}

fn internal(error: impl std::fmt::Display) -> (StatusCode, String) {
    tracing::error!(%error, "company directory failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "couldn't load companies".into(),
    )
}

pub async fn authorized_companies(
    pool: &db::Pool,
    role: &Role,
    mode: RoutingMode,
) -> Result<Vec<CompanyAccess>, skilj_core::error::Error> {
    if role.status != skilj_core::access_control::RoleStatus::Active {
        return Ok(Vec::new());
    }
    let tenants: BTreeMap<_, _> = recorded_company_tenants(pool).await?.into_iter().collect();
    let mut companies = BTreeMap::new();
    for event in db::list_events(pool, BOUNDED_CONTEXT, "CompanySignedUp").await? {
        if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.payload) {
            if let Some(company) = payload["company_id"].as_str() {
                let context = if mode == RoutingMode::Tenant {
                    tenants
                        .get(company)
                        .map(String::as_str)
                        .unwrap_or(BOUNDED_CONTEXT)
                } else {
                    BOUNDED_CONTEXT
                };
                companies.insert(company.to_owned(), context.to_owned());
            }
        }
    }
    let mut allowed = Vec::new();
    for (company_id, bounded_context) in companies {
        if let Some(mapping) =
            db::get_active_role_access_mapping(pool, &role.id, &bounded_context).await?
        {
            if mapping.bounded_context.status
                != skilj_core::event_store::BoundedContextStatus::Active
            {
                continue;
            }
            if mapping
                .scope
                .as_deref()
                .is_none_or(|scope| scope == company_id)
            {
                allowed.push(CompanyAccess {
                    company_id,
                    bounded_context,
                });
            }
        }
    }
    Ok(allowed)
}
