//! `/resources`.
//!
//! Promoted to a top-level collection in 3.1; in 3.0 it hung off `/vens/{id}/resources`. A resource
//! is owned through its VEN, so every authorization decision here resolves the parent first.

use axum::{
    extract::{Path, RawQuery, State},
    http::HeaderMap,
    response::Response,
};

use crate::model::{ObjectId, ObjectType, Resource, ResourceName, ResourceRequest};

use crate::schema::PayloadGroup;

use super::super::{ApiError, AppState, auth::Scope, store::ResourceQuery};
use super::{Ctx, JsonBody, check_attributes, created, fanout, ok, params, parse_id};

/// `GET /resources`
pub async fn list(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    ctx.require_read()?;
    let p = params(&raw);

    let resources = state
        .storage
        .list_resources(&ResourceQuery {
            ven_id: p.first("venID").map(parse_id).transpose()?,
            resource_name: p.parse_one::<ResourceName>("resourceName")?,
            access: ctx.owned_access(p.targets()?),
            page: p.page()?,
        })
        .await?;
    ok(&state, &headers, &resources)
}

/// `POST /resources`
pub async fn create(
    State(state): State<AppState>,
    ctx: Ctx,
    body: JsonBody,
) -> Result<Response, ApiError> {
    ctx.require(Scope::WriteVens)?;
    let request: ResourceRequest = body.parse()?;
    check_attributes(&state, request.attributes(), PayloadGroup::VenAttribute)?;
    let now = state.clock.now();
    let fanout = fanout(&state, ObjectType::Resource).await;
    let resource = build(&state, &ctx, request, now, None).await?;
    let resource = state.storage.create_resource(resource, &fanout).await?;
    created(&resource)
}

/// `GET /resources/{id}`
pub async fn get(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    ctx.require_read()?;
    let id = parse_id(&id)?;
    let resource = state.storage.get_resource(&id).await?;
    require_owner(&state, &ctx, &resource, &id).await?;
    ok(&state, &headers, &resource)
}

/// `PUT /resources/{id}`
pub async fn update(
    State(state): State<AppState>,
    ctx: Ctx,
    Path(id): Path<String>,
    body: JsonBody,
) -> Result<Response, ApiError> {
    ctx.require(Scope::WriteVens)?;
    let id = parse_id(&id)?;
    let existing = state.storage.get_resource(&id).await?;
    require_owner(&state, &ctx, &existing, &id).await?;

    let request: ResourceRequest = body.parse()?;
    check_attributes(&state, request.attributes(), PayloadGroup::VenAttribute)?;
    let fanout = fanout(&state, ObjectType::Resource).await;
    let resource = build(&state, &ctx, request, state.clock.now(), Some(&existing)).await?;
    let resource = state
        .storage
        .update_resource(&id, resource, &fanout)
        .await?;
    ok(&state, &HeaderMap::new(), &resource)
}

/// `DELETE /resources/{id}`
pub async fn delete(
    State(state): State<AppState>,
    ctx: Ctx,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    ctx.require(Scope::WriteVens)?;
    let id = parse_id(&id)?;
    let fanout = fanout(&state, ObjectType::Resource).await;
    let existing = state.storage.get_resource(&id).await?;
    require_owner(&state, &ctx, &existing, &id).await?;
    let resource = state.storage.delete_resource(&id, &fanout).await?;
    ok(&state, &HeaderMap::new(), &resource)
}

async fn build(
    state: &AppState,
    ctx: &Ctx,
    request: ResourceRequest,
    now: crate::model::Timestamp,
    existing: Option<&Resource>,
) -> Result<Resource, ApiError> {
    let placeholder = ObjectId::new("pending").expect("literal is a valid id");
    match request {
        ResourceRequest::Bl(bl) => {
            if !ctx.is_business_logic() {
                return Err(ApiError::Forbidden(
                    "only business logic may send a BL_RESOURCE_REQUEST".into(),
                ));
            }
            // 3.1.0 required `clientID` here and 3.1.1 removed it as redundant with `venID`. It is
            // still accepted, and it is still *checked*: a field that is read and then ignored is
            // the shape of D-045, and one naming a different client than the parent VEN's owner is
            // either a bug in the caller or a mistake about which VEN this resource joins.
            if let Some(claimed) = &bl.client_id {
                let owner = state.storage.get_ven(&bl.ven_id).await?.client_id;
                if claimed != &owner {
                    return Err(ApiError::BadRequest(format!(
                        "clientID {claimed} does not own venID {}; the resource's owner is its \
                         VEN's owner ({owner})",
                        bl.ven_id
                    )));
                }
            }
            Ok(Resource {
                id: existing.map(|r| r.id.clone()).unwrap_or(placeholder),
                created_date_time: existing.map(|r| r.created_date_time).unwrap_or(now),
                modification_date_time: now,
                object_type: ObjectType::Resource,
                resource_name: bl.resource_name,
                ven_id: bl.ven_id,
                targets: bl.targets,
                attributes: bl.attributes,
            })
        }
        ResourceRequest::Ven(v) => {
            // A VEN-written resource belongs to the caller's own VEN. 3.1.0 required the body to
            // repeat that id, so it is accepted — but only when it agrees, because a body naming
            // somebody else's VEN is either a bug or an attempt, and neither deserves silence.
            let ven_id = own_ven_id(state, ctx).await?.ok_or_else(|| {
                ApiError::Forbidden(
                    "this client has no VEN object; create one before adding resources".into(),
                )
            })?;
            if let Some(claimed) = &v.ven_id
                && claimed != &ven_id
            {
                return Err(ApiError::Forbidden(format!(
                    "venID {claimed} is not this client's VEN"
                )));
            }
            Ok(Resource {
                id: existing.map(|r| r.id.clone()).unwrap_or(placeholder),
                created_date_time: existing.map(|r| r.created_date_time).unwrap_or(now),
                modification_date_time: now,
                object_type: ObjectType::Resource,
                resource_name: v.resource_name,
                ven_id,
                targets: existing.map(|e| e.targets.clone()).unwrap_or_default(),
                attributes: v.attributes,
            })
        }
    }
}

/// The VEN object belonging to the caller, if any.
async fn own_ven_id(state: &AppState, ctx: &Ctx) -> Result<Option<ObjectId>, ApiError> {
    let Some(client_id) = ctx.principal.client_id.as_ref() else {
        return Ok(None);
    };
    Ok(state
        .storage
        .get_ven_by_client(client_id)
        .await?
        .map(|v| v.id))
}

/// A resource is owned through its VEN, so the parent decides.
async fn require_owner(
    state: &AppState,
    ctx: &Ctx,
    resource: &Resource,
    id: &ObjectId,
) -> Result<(), ApiError> {
    let ven = state.storage.get_ven(&resource.ven_id).await.ok();
    super::require_owner(
        &ctx.owned_id_access(),
        ven.as_ref().map(|v| &v.client_id),
        ObjectType::Resource,
        id,
    )
}
