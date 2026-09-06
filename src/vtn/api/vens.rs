//! `/vens`.
//!
//! The `objectType` discriminator on the request body is what keeps a VEN from granting itself
//! targets: `VEN_VEN_REQUEST` has no `targets` member, so the privilege is unreachable rather than
//! merely unauthorised. This module enforces the other half — that only business logic may *send*
//! the BL-flavoured body.

use axum::{
    extract::{Path, RawQuery, State},
    http::HeaderMap,
    response::Response,
};

use crate::model::{ObjectId, ObjectType, Ven, VenName, VenRequest};

use crate::schema::PayloadGroup;

use super::super::{ApiError, AppState, auth::Scope, store::VenQuery};
use super::{
    Ctx, JsonBody, check_attributes, created, fanout, ok, params, parse_id, require_owner,
};

/// `GET /vens`
pub async fn list(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    ctx.require_read()?;
    let p = params(&raw);

    let vens = state
        .storage
        .list_vens(&VenQuery {
            ven_name: p.parse_one::<VenName>("venName")?,
            access: ctx.owned_access(p.targets()?),
            page: p.page()?,
        })
        .await?;
    ok(&state, &headers, &vens)
}

/// `POST /vens`
pub async fn create(
    State(state): State<AppState>,
    ctx: Ctx,
    body: JsonBody,
) -> Result<Response, ApiError> {
    ctx.require(Scope::WriteVens)?;
    let request: VenRequest = body.parse()?;
    check_attributes(&state, request.attributes(), PayloadGroup::VenAttribute)?;
    let now = state.clock.now();
    let fanout = fanout(&state, ObjectType::Ven).await;
    let ven = build(&ctx, request, now, None)?;
    let ven = state.storage.create_ven(ven, &fanout).await?;
    created(&ven)
}

/// `GET /vens/{id}`
pub async fn get(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    ctx.require_read()?;
    let id = parse_id(&id)?;
    let ven = state.storage.get_ven(&id).await?;
    require_owner(
        &ctx.owned_id_access(),
        Some(&ven.client_id),
        ObjectType::Ven,
        &id,
    )?;
    ok(&state, &headers, &ven)
}

/// `PUT /vens/{id}`
pub async fn update(
    State(state): State<AppState>,
    ctx: Ctx,
    Path(id): Path<String>,
    body: JsonBody,
) -> Result<Response, ApiError> {
    ctx.require(Scope::WriteVens)?;
    let id = parse_id(&id)?;
    let existing = state.storage.get_ven(&id).await?;
    require_owner(
        &ctx.owned_id_access(),
        Some(&existing.client_id),
        ObjectType::Ven,
        &id,
    )?;

    let request: VenRequest = body.parse()?;
    check_attributes(&state, request.attributes(), PayloadGroup::VenAttribute)?;
    let fanout = fanout(&state, ObjectType::Ven).await;
    let ven = build(&ctx, request, state.clock.now(), Some(&existing))?;
    let ven = state.storage.update_ven(&id, ven, &fanout).await?;
    ok(&state, &HeaderMap::new(), &ven)
}

/// `DELETE /vens/{id}`
pub async fn delete(
    State(state): State<AppState>,
    ctx: Ctx,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    ctx.require(Scope::WriteVens)?;
    let id = parse_id(&id)?;
    let fanout = fanout(&state, ObjectType::Ven).await;
    let existing = state.storage.get_ven(&id).await?;
    require_owner(
        &ctx.owned_id_access(),
        Some(&existing.client_id),
        ObjectType::Ven,
        &id,
    )?;
    let ven = state.storage.delete_ven(&id, &fanout).await?;
    ok(&state, &HeaderMap::new(), &ven)
}

/// Turn a request body into the object to store, enforcing who may say what.
fn build(
    ctx: &Ctx,
    request: VenRequest,
    now: crate::model::Timestamp,
    existing: Option<&Ven>,
) -> Result<Ven, ApiError> {
    let placeholder = ObjectId::new("pending").expect("literal is a valid id");
    match request {
        VenRequest::Bl(bl) => {
            if !ctx.is_business_logic() {
                return Err(ApiError::Forbidden(
                    "only business logic may send a BL_VEN_REQUEST; use VEN_VEN_REQUEST".into(),
                ));
            }
            Ok(Ven {
                id: existing.map(|v| v.id.clone()).unwrap_or(placeholder),
                created_date_time: existing.map(|v| v.created_date_time).unwrap_or(now),
                modification_date_time: now,
                object_type: ObjectType::Ven,
                client_id: bl.client_id,
                ven_name: bl.ven_name,
                targets: bl.targets,
                attributes: bl.attributes,
            })
        }
        VenRequest::Ven(v) => {
            let client_id = ctx.client_id()?.clone();
            Ok(Ven {
                id: existing.map(|v| v.id.clone()).unwrap_or(placeholder),
                created_date_time: existing.map(|v| v.created_date_time).unwrap_or(now),
                modification_date_time: now,
                object_type: ObjectType::Ven,
                client_id,
                ven_name: v.ven_name,
                // Targets are granted by business logic, never self-assigned. On update, whatever
                // was granted stays granted.
                targets: existing.map(|e| e.targets.clone()).unwrap_or_default(),
                attributes: v.attributes,
            })
        }
    }
}
