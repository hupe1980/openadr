//! `/reports`.
//!
//! Reports are owned objects: a VEN sees only the ones it created. The specification also says only
//! VENs write reports, which leaves a report unreachable once its VEN's credentials are revoked, so
//! business logic may delete but not create — the smallest deviation that keeps the data manageable.

use axum::{
    extract::{Path, RawQuery, State},
    http::HeaderMap,
    response::Response,
};

use crate::model::{ClientName, ObjectType, ReportRequest};
use crate::schema::PayloadGroup;

use super::super::{ApiError, AppState, auth::Scope, store::ReportQuery};
use super::{Ctx, JsonBody, check_payloads, created, fanout, ok, params, parse_id, require_owner};

/// `GET /reports`
pub async fn list(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    ctx.require_read()?;
    let p = params(&raw);

    let reports = state
        .storage
        .list_reports(&ReportQuery {
            program_id: p.first("programID").map(parse_id).transpose()?,
            event_id: p.first("eventID").map(parse_id).transpose()?,
            client_name: p.parse_one::<ClientName>("clientName")?,
            access: ctx.owned_access(Vec::new()),
            page: p.page()?,
        })
        .await?;
    ok(&state, &headers, &reports)
}

/// `POST /reports`
pub async fn create(
    State(state): State<AppState>,
    ctx: Ctx,
    body: JsonBody,
) -> Result<Response, ApiError> {
    ctx.require(Scope::WriteReports)?;
    if ctx.is_business_logic() {
        return Err(ApiError::Forbidden(
            "reports are written by VENs; business logic may read and delete them".into(),
        ));
    }
    let request: ReportRequest = body.parse()?;
    check_report_payloads(&state, &request)?;
    // The VTN stamps the identity; a client cannot claim someone else's.
    let owner = ctx.client_id()?.clone();
    let fanout = fanout(&state, ObjectType::Report).await;
    let report = state
        .storage
        .create_report(request, Some(owner), state.clock.now(), &fanout)
        .await?;
    created(&report)
}

/// `GET /reports/{id}`
pub async fn get(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    ctx.require_read()?;
    let id = parse_id(&id)?;
    let report = state.storage.get_report(&id).await?;
    require_owner(
        &ctx.owned_id_access(),
        report.client_id.as_ref(),
        ObjectType::Report,
        &id,
    )?;
    ok(&state, &headers, &report)
}

/// `PUT /reports/{id}`
pub async fn update(
    State(state): State<AppState>,
    ctx: Ctx,
    Path(id): Path<String>,
    body: JsonBody,
) -> Result<Response, ApiError> {
    ctx.require(Scope::WriteReports)?;
    let id = parse_id(&id)?;
    let existing = state.storage.get_report(&id).await?;
    require_owner(
        &ctx.owned_id_access(),
        existing.client_id.as_ref(),
        ObjectType::Report,
        &id,
    )?;
    if ctx.is_business_logic() {
        return Err(ApiError::Forbidden(
            "reports are written by the VEN that created them".into(),
        ));
    }
    let request: ReportRequest = body.parse()?;
    // The same check `create` makes. A `PUT` that skipped it was a way round `Policy::Strict`:
    // post an empty report, then replace it with the payloads the policy refuses.
    check_report_payloads(&state, &request)?;
    let fanout = fanout(&state, ObjectType::Report).await;
    let report = state
        .storage
        .update_report(&id, request, state.clock.now(), &fanout)
        .await?;
    ok(&state, &HeaderMap::new(), &report)
}

/// `DELETE /reports/{id}`
pub async fn delete(
    State(state): State<AppState>,
    ctx: Ctx,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    ctx.require(Scope::WriteReports)?;
    let id = parse_id(&id)?;
    let fanout = fanout(&state, ObjectType::Report).await;
    let existing = state.storage.get_report(&id).await?;
    require_owner(
        &ctx.owned_id_access(),
        existing.client_id.as_ref(),
        ObjectType::Report,
        &id,
    )?;
    let report = state.storage.delete_report(&id, &fanout).await?;
    ok(&state, &HeaderMap::new(), &report)
}

/// Check every interval of every resource against the report enumerations.
///
/// One function, called by `create` and by `update`, because a validation rule that only one of
/// the two write paths applies is a validation rule with a way round it.
fn check_report_payloads(state: &AppState, request: &ReportRequest) -> Result<(), ApiError> {
    for resource in &request.resources {
        for interval in &resource.intervals {
            check_payloads(state, &interval.payloads, PayloadGroup::Report)?;
        }
    }
    Ok(())
}
