//! `/programs`.

use axum::{
    extract::{Path, RawQuery, State},
    http::HeaderMap,
    response::Response,
};

use crate::model::{ProgramName, ProgramRequest};

use super::super::{ApiError, AppState, auth::Scope, store::ProgramQuery};
use super::{
    Ctx, JsonBody, check_attributes, created, fanout, hide_targets, ok, params, parse_id,
    require_visible,
};
use crate::model::ObjectType;
use crate::schema::PayloadGroup;

/// `GET /programs`
pub async fn list(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    ctx.require_read()?;
    let p = params(&raw);
    let targets = p.targets()?;

    let program_name = if state.config.program_name_lookup {
        p.parse_one::<ProgramName>("programName")?
    } else {
        None
    };

    let access = ctx.list_access(targets).await;
    let mut programs = state
        .storage
        .list_programs(&ProgramQuery {
            program_name,
            access: access.clone(),
            page: p.page()?,
        })
        .await?;

    // Storage already applied `access` before paginating; this only narrows what each page shows.
    hide_targets(
        &access,
        &mut programs,
        |p| &p.content.targets,
        |p, t| p.content.targets = t,
    );
    ok(&state, &headers, &programs)
}

/// `POST /programs`
pub async fn create(
    State(state): State<AppState>,
    ctx: Ctx,
    body: JsonBody,
) -> Result<Response, ApiError> {
    ctx.require(Scope::WritePrograms)?;
    let request: ProgramRequest = body.parse()?;
    check_attributes(
        &state,
        request.attributes.as_ref(),
        PayloadGroup::ProgramAttribute,
    )?;
    let fanout = fanout(&state, ObjectType::Program).await;
    let program = state
        .storage
        .create_program(request, state.clock.now(), &fanout)
        .await?;
    created(&program)
}

/// `GET /programs/{id}`
pub async fn get(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    ctx.require_read()?;
    let id = parse_id(&id)?;
    let program = state.storage.get_program(&id).await?;
    let program = require_visible(
        &ctx.id_access().await,
        program,
        ObjectType::Program,
        &id,
        |p| &p.content.targets,
        |p, t| p.content.targets = t,
    )?;
    ok(&state, &headers, &program)
}

/// `PUT /programs/{id}`
pub async fn update(
    State(state): State<AppState>,
    ctx: Ctx,
    Path(id): Path<String>,
    body: JsonBody,
) -> Result<Response, ApiError> {
    ctx.require(Scope::WritePrograms)?;
    let id = parse_id(&id)?;
    let request: ProgramRequest = body.parse()?;
    check_attributes(
        &state,
        request.attributes.as_ref(),
        PayloadGroup::ProgramAttribute,
    )?;
    let fanout = fanout(&state, ObjectType::Program).await;
    let program = state
        .storage
        .update_program(&id, request, state.clock.now(), &fanout)
        .await?;
    ok(&state, &HeaderMap::new(), &program)
}

/// `DELETE /programs/{id}`
pub async fn delete(
    State(state): State<AppState>,
    ctx: Ctx,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    ctx.require(Scope::WritePrograms)?;
    let fanout = fanout(&state, ObjectType::Program).await;
    let id = parse_id(&id)?;
    let program = state.storage.delete_program(&id, &fanout).await?;
    ok(&state, &HeaderMap::new(), &program)
}
