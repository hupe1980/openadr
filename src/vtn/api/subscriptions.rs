//! `/subscriptions`.

use axum::{
    extract::{Path, RawQuery, State},
    http::HeaderMap,
    response::Response,
};

use crate::model::{ClientName, ObjectType, SubscriptionRequest};

use super::super::{ApiError, AppState, auth::Scope, store::OwnerKind, store::SubscriptionQuery};
use super::{Ctx, JsonBody, created, fanout_owned, ok, params, parse_id, require_owner};

/// `GET /subscriptions`
pub async fn list(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    ctx.require_read()?;
    let p = params(&raw);

    let subscriptions = state
        .storage
        .list_subscriptions(&SubscriptionQuery {
            program_id: p.first("programID").map(parse_id).transpose()?,
            client_name: p.parse_one::<ClientName>("clientName")?,
            objects: p.object_types()?,
            access: ctx.owned_access(Vec::new()),
            page: p.page()?,
        })
        .await?;
    ok(&state, &headers, &subscriptions)
}

/// `POST /subscriptions`
pub async fn create(
    State(state): State<AppState>,
    ctx: Ctx,
    body: JsonBody,
) -> Result<Response, ApiError> {
    ctx.require(Scope::WriteSubscriptions)?;
    require_deliverable(&state)?;
    let request: SubscriptionRequest = body.parse()?;
    validate(&request, &state.config.callback_policy)?;
    challenge(&state, &request).await?;
    let owner = ctx.client_id()?.clone();
    // What the creator *is*, recorded now because the fan-out cannot ask later: it runs from a
    // dispatcher with no credential, and a business-logic subscriber mistaken for a VEN holds an
    // empty grant and is therefore told about no targeted object at all.
    let owner_kind = if ctx.is_business_logic() {
        OwnerKind::BusinessLogic
    } else {
        OwnerKind::Ven
    };
    let fanout = fanout_owned(&state, ObjectType::Subscription, Some(&owner)).await;
    let subscription = state
        .storage
        .create_subscription(request, owner, owner_kind, state.clock.now(), &fanout)
        .await?;
    created(&subscription)
}

/// `GET /subscriptions/{id}`
pub async fn get(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    ctx.require_read()?;
    let id = parse_id(&id)?;
    let subscription = state.storage.get_subscription(&id).await?;
    require_owner(
        &ctx.owned_id_access(),
        Some(&subscription.client_id),
        ObjectType::Subscription,
        &id,
    )?;
    ok(&state, &headers, &subscription)
}

/// `PUT /subscriptions/{id}`
pub async fn update(
    State(state): State<AppState>,
    ctx: Ctx,
    Path(id): Path<String>,
    body: JsonBody,
) -> Result<Response, ApiError> {
    ctx.require(Scope::WriteSubscriptions)?;
    let id = parse_id(&id)?;
    let existing = state.storage.get_subscription(&id).await?;
    require_owner(
        &ctx.owned_id_access(),
        Some(&existing.client_id),
        ObjectType::Subscription,
        &id,
    )?;
    require_deliverable(&state)?;
    let request: SubscriptionRequest = body.parse()?;
    validate(&request, &state.config.callback_policy)?;
    challenge(&state, &request).await?;
    let fanout = fanout_owned(&state, ObjectType::Subscription, Some(&existing.client_id)).await;
    let subscription = state
        .storage
        .update_subscription(&id, request, state.clock.now(), &fanout)
        .await?;
    ok(&state, &HeaderMap::new(), &subscription)
}

/// `DELETE /subscriptions/{id}`
pub async fn delete(
    State(state): State<AppState>,
    ctx: Ctx,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    ctx.require(Scope::WriteSubscriptions)?;
    let id = parse_id(&id)?;
    let existing = state.storage.get_subscription(&id).await?;
    require_owner(
        &ctx.owned_id_access(),
        Some(&existing.client_id),
        ObjectType::Subscription,
        &id,
    )?;
    // After the read, because the snapshot is keyed on the object's owner — and still before the
    // write, which is the only ordering the transactional outbox requires.
    let fanout = fanout_owned(&state, ObjectType::Subscription, Some(&existing.client_id)).await;
    let subscription = state.storage.delete_subscription(&id, &fanout).await?;
    ok(&state, &HeaderMap::new(), &subscription)
}

/// Refuse a subscription this VTN could never deliver.
///
/// A subscription is a standing request for notifications. Accepting one on a VTN with no webhook
/// transport creates an object that will never produce a delivery, and OpenADR has no way to tell
/// the subscriber that — it would simply wait. `501` says so at the only moment the subscriber is
/// listening. `GET /notifiers` reports the same fact ahead of time.
fn require_deliverable(state: &AppState) -> Result<(), ApiError> {
    if state.notifier.handles(crate::vtn::notify::Channel::Webhook) {
        Ok(())
    } else {
        Err(ApiError::NotImplemented(
            "this VTN has no webhook transport configured, so a subscription could never be \
             delivered; see GET /notifiers"
                .into(),
        ))
    }
}

/// A subscription must be actionable, and its callback must be one the transport will use.
///
/// The URL rule is [`CallbackPolicy`](crate::vtn::notify::CallbackPolicy) — the same object the
/// webhook transport consults before every delivery. Writing it out again here is how the two
/// moments drift apart, so this checks the literal and the transport re-checks what it resolves to.
fn validate(
    request: &SubscriptionRequest,
    policy: &crate::vtn::notify::CallbackPolicy,
) -> Result<(), ApiError> {
    if request.object_operations.is_empty() {
        return Err(ApiError::BadRequest(
            "a subscription needs at least one objectOperations entry".into(),
        ));
    }
    for op in &request.object_operations {
        if op.objects.is_empty() {
            return Err(ApiError::BadRequest(
                "an objectOperations entry needs at least one object type".into(),
            ));
        }
        if op.operations.is_empty() {
            return Err(ApiError::BadRequest(
                "an objectOperations entry needs at least one operation".into(),
            ));
        }
        policy
            .check_literal(&op.callback_url)
            .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    }
    Ok(())
}

/// Prove the subscriber controls every callback URL it named.
///
/// `[Def §Webhooks]` requires the echo challenge *before* the subscription exists, which is what
/// makes it a defence: a subscription pointed at a third party is never created, rather than being
/// created and then failing to deliver. It costs one round trip per distinct callback URL on a
/// write that happens once per subscriber, and the transport decides what proving means — a VTN
/// with no webhook transport has nothing to prove and accepts.
async fn challenge(state: &AppState, request: &SubscriptionRequest) -> Result<(), ApiError> {
    let mut seen: Vec<&str> = Vec::new();
    for op in &request.object_operations {
        if seen.contains(&op.callback_url.as_str()) {
            continue;
        }
        seen.push(&op.callback_url);
        state
            .notifier
            .verify_callback(&op.callback_url)
            .await
            .map_err(|e| {
                ApiError::BadRequest(format!(
                    "callbackUrl {} did not pass the echo challenge: {e}",
                    op.callback_url
                ))
            })?;
    }
    Ok(())
}
