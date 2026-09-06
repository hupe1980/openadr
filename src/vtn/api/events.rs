//! `/events`.

use axum::{
    extract::{Path, RawQuery, State},
    http::HeaderMap,
    response::Response,
};

use crate::core::IntervalExpander;
use crate::model::{Duration, Event, EventRequest, IntervalPeriod, ObjectType};
use crate::schema::PayloadGroup;

use super::super::{ApiError, AppState, auth::Scope, store::EventQuery};
use super::{
    Ctx, JsonBody, check_payloads, created, fanout, hide_targets, ok, params, parse_id,
    require_visible,
};

/// `GET /events`
pub async fn list(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    ctx.require_read()?;
    let p = params(&raw);

    // `?active=true` is resolved against each event's stored window, inside the query, so that it
    // narrows the set *before* pagination cuts a page from it.
    let active_at = match p.active()? {
        Some(true) => Some(state.clock.now()),
        _ => None,
    };

    let access = ctx.list_access(p.targets()?).await;
    let mut events = state
        .storage
        .list_events(&EventQuery {
            program_id: p.first("programID").map(parse_id).transpose()?,
            active_at,
            access: access.clone(),
            page: p.page()?,
        })
        .await?;

    // Storage already applied `access`; this only narrows the targets each object shows.
    hide_targets(
        &access,
        &mut events,
        |e| &e.content.targets,
        |e, t| e.content.targets = t,
    );
    ok(&state, &headers, &events)
}

/// `POST /events`
pub async fn create(
    State(state): State<AppState>,
    ctx: Ctx,
    body: JsonBody,
) -> Result<Response, ApiError> {
    ctx.require(Scope::WriteEvents)?;
    let request: EventRequest = body.parse()?;
    validate_event(&state, &request)?;
    let fanout = fanout(&state, ObjectType::Event).await;
    let event = state
        .storage
        .create_event(request, state.clock.now(), &fanout)
        .await?;
    created(&event)
}

/// `GET /events/{id}`
pub async fn get(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    ctx.require_read()?;
    let id = parse_id(&id)?;
    let event: Event = require_visible(
        &ctx.id_access().await,
        state.storage.get_event(&id).await?,
        ObjectType::Event,
        &id,
        |e| &e.content.targets,
        |e, t| e.content.targets = t,
    )?;
    ok(&state, &headers, &event)
}

/// `PUT /events/{id}`
pub async fn update(
    State(state): State<AppState>,
    ctx: Ctx,
    Path(id): Path<String>,
    body: JsonBody,
) -> Result<Response, ApiError> {
    ctx.require(Scope::WriteEvents)?;
    let id = parse_id(&id)?;
    let request: EventRequest = body.parse()?;
    validate_event(&state, &request)?;
    let fanout = fanout(&state, ObjectType::Event).await;
    let event = state
        .storage
        .update_event(&id, request, state.clock.now(), &fanout)
        .await?;
    ok(&state, &HeaderMap::new(), &event)
}

/// `DELETE /events/{id}`
///
/// Deleting an event is how the specification cancels one; subscribers see a `DELETE` notification.
pub async fn delete(
    State(state): State<AppState>,
    ctx: Ctx,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    ctx.require(Scope::WriteEvents)?;
    let fanout = fanout(&state, ObjectType::Event).await;
    let id = parse_id(&id)?;
    let event = state.storage.delete_event(&id, &fanout).await?;
    ok(&state, &HeaderMap::new(), &event)
}

/// Refuse an event whose intervals cannot be resolved to absolute time.
///
/// The specification places content validation on the client, but an interval with no start and
/// nothing to inherit one from cannot be placed in time by any VEN. Catching it here turns a silent
/// field failure into an error the publisher sees at publication.
fn validate_event(state: &AppState, request: &EventRequest) -> Result<(), ApiError> {
    if let Some(intervals) = &request.intervals {
        for interval in intervals {
            check_payloads(state, &interval.payloads, PayloadGroup::Event)?;
        }
    }
    check_durations_run_forwards(request)?;
    // The sequence rather than one pass: it resolves the implied interval structure as well, so an
    // `intervalPeriod` whose duration cannot be added to its start is caught here too.
    IntervalExpander::at(state.clock.now())
        .sequence(request)
        .map_err(|e| {
            ApiError::BadRequest(format!("the event's intervals are not resolvable: {e}"))
        })?;
    Ok(())
}

/// Refuse an event carrying a duration that runs backwards.
///
/// The schema's `duration` pattern begins `^(-?)P`, so `-PT1H` is well formed and the wire model
/// parses it — deliberately, because a client has to be able to report what a peer sent (see
/// [`Duration::is_negative`](crate::model::Duration::is_negative)). Nothing OpenADR uses a duration
/// for has a backwards reading, though, and such an event is *inert*: its active window ends before
/// it starts, so `?active=true` never returns it and no VEN following the collection ever sees it.
/// A `201` and a dispatch that never dispatches is the failure this refusal exists to prevent
/// (D-126).
fn check_durations_run_forwards(request: &EventRequest) -> Result<(), ApiError> {
    if request.duration.as_ref().is_some_and(Duration::is_negative) {
        return Err(backwards("duration"));
    }
    check_period(request.interval_period.as_ref(), "intervalPeriod")?;
    for (index, interval) in request.intervals.iter().flatten().enumerate() {
        check_period(
            interval.interval_period.as_ref(),
            &format!("intervals[{index}].intervalPeriod"),
        )?;
    }
    Ok(())
}

fn check_period(period: Option<&IntervalPeriod>, field: &str) -> Result<(), ApiError> {
    let Some(period) = period else {
        return Ok(());
    };
    if period.duration.as_ref().is_some_and(Duration::is_negative) {
        return Err(backwards(&format!("{field}.duration")));
    }
    if period
        .randomize_start
        .as_ref()
        .is_some_and(Duration::is_negative)
    {
        return Err(backwards(&format!("{field}.randomizeStart")));
    }
    Ok(())
}

fn backwards(field: &str) -> ApiError {
    ApiError::BadRequest(format!(
        "{field} runs backwards; a duration in OpenADR is a length, and an event carrying a \
         negative one is never active"
    ))
}
