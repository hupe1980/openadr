//! Object privacy.
//!
//! OpenADR 3.1's privacy model has two halves (Definitions §Object Privacy):
//!
//! * **Ownership.** A VEN reads only the `ven`, `resource`, `subscription` and `report` objects
//!   whose `clientID` matches its own. Business logic reads everything.
//! * **Targeting.** Business logic *grants* targets to a VEN by writing them on that VEN's `ven` and
//!   `resource` objects. A `program` or `event` carrying targets is then readable only by a VEN
//!   whose grant intersects both the object's targets and the targets it asked for. The response
//!   shows only the targets the reader asked for — never the object's full set.
//!
//! Both halves live in [`Access`]: one type, constructed once per request, consulted by the storage
//! query *before* pagination and by the handler afterwards for target hiding. Anything that decides
//! who may see what goes through it, because a rule implemented in three places is a rule with three
//! chances to leak a competitor's dispatch schedule.
//!
//! The two questions it answers are not independent: [`Access::visible_targets`] is *defined in
//! terms of* [`Access::target_filter`], which is the shape a SQL backend renders. There is one
//! predicate, so the database's answer and the handler's cannot drift.
//!
//! Guide: <https://hupe1980.github.io/openadr/docs/object-privacy/>.

use crate::std_shim::{BTreeMap, Vec};

use crate::model::{ClientId, Target};

/// What a client is allowed to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// Business logic: unrestricted read access.
    BusinessLogic,
    /// A VEN, identified by its client id and holding a set of granted targets.
    Ven {
        /// The identity the VTN stamps on objects this client creates.
        client_id: ClientId,
        /// Targets granted through the client's `ven` and `resource` objects.
        grant: Grant,
    },
    /// An unauthenticated reader of a public tariff server.
    ///
    /// Treated as a VEN with an empty grant: untargeted objects are readable, targeted ones are not.
    Anonymous,
}

impl Role {
    /// The client identity, if this role has one.
    pub fn client_id(&self) -> Option<&ClientId> {
        match self {
            Role::Ven { client_id, .. } => Some(client_id),
            _ => None,
        }
    }

    /// Whether this role reads without restriction.
    pub fn is_business_logic(&self) -> bool {
        matches!(self, Role::BusinessLogic)
    }

    /// The granted targets, empty for business logic and anonymous callers.
    pub fn grant(&self) -> &Grant {
        match self {
            Role::Ven { grant, .. } => grant,
            _ => Grant::EMPTY_REF,
        }
    }
}

/// The targets a client has been granted.
///
/// The union of the `targets` on the client's `ven` object and on every `resource` belonging to it —
/// the specification's "logical AND of the targets in the request and of the ven object and
/// associated resource objects".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grant {
    targets: Vec<Target>,
}

impl Grant {
    const EMPTY_REF: &'static Grant = &Grant {
        targets: Vec::new(),
    };

    /// An empty grant.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build from an iterator, sorting and de-duplicating.
    pub fn from_targets(targets: impl IntoIterator<Item = Target>) -> Self {
        let mut grant = Self::default();
        grant.extend(targets);
        grant
    }

    /// The granted targets, sorted.
    pub fn targets(&self) -> &[Target] {
        &self.targets
    }

    /// Whether anything has been granted.
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// Whether a specific target is granted.
    pub fn contains(&self, target: &Target) -> bool {
        self.targets.binary_search(target).is_ok()
    }

    /// Add targets, keeping the set sorted and unique.
    pub fn extend(&mut self, targets: impl IntoIterator<Item = Target>) {
        self.targets.extend(targets);
        self.targets.sort();
        self.targets.dedup();
    }
}

/// How an empty target list is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Empty {
    /// Naming no targets hides every targeted object.
    ///
    /// What the specification requires of `GET /programs` and `GET /events`: "VENs may only read
    /// objects with targets by providing matching targets".
    HidesTargeted,
    /// Naming no targets means "everything I have been granted".
    ///
    /// For a read by id — the id *is* the request — and for push delivery, where a subscription with
    /// no targets is a request for everything the subscriber is entitled to, not for nothing.
    MeansEverythingGranted,
}

/// Who is asking, and what they asked for.
///
/// Built once per request and passed *into* the storage query, so that filtering happens before
/// pagination. Filtering a page after it has been cut produces short pages and silently lost
/// records: a VEN entitled to one of two requested targets would see a handful of its events and a
/// client that stops on a short page would miss the rest.
#[derive(Debug, Clone)]
pub struct Access {
    role: Role,
    requested: Vec<Target>,
    empty: Empty,
}

impl Access {
    /// Access for a collection read, where targets must be named explicitly.
    pub fn list(role: Role, requested: Vec<Target>) -> Self {
        Self {
            role,
            requested,
            empty: Empty::HidesTargeted,
        }
    }

    /// Access for a read by id, where the id is the request.
    ///
    /// The grant is still intersected, so guessing ids reveals nothing, and target hiding still
    /// applies to the response.
    pub fn by_id(role: Role) -> Self {
        Self {
            role,
            requested: Vec::new(),
            empty: Empty::MeansEverythingGranted,
        }
    }

    /// Access for push delivery to a subscriber.
    pub fn push(role: Role, subscription_targets: Vec<Target>) -> Self {
        Self {
            role,
            requested: subscription_targets,
            empty: Empty::MeansEverythingGranted,
        }
    }

    /// Unrestricted access, for internal queries that must see everything.
    pub fn unrestricted() -> Self {
        Self::by_id(Role::BusinessLogic)
    }

    /// The caller's role.
    pub fn role(&self) -> &Role {
        &self.role
    }

    /// The targets the caller asked for.
    pub fn requested(&self) -> &[Target] {
        &self.requested
    }

    /// Whether the caller is business logic.
    pub fn is_business_logic(&self) -> bool {
        self.role.is_business_logic()
    }

    /// The caller's client id, if it has one.
    pub fn client_id(&self) -> Option<&ClientId> {
        self.role.client_id()
    }

    /// Whether a targeted object is visible at all.
    ///
    /// Defined in terms of [`Access::target_filter`] so that the predicate a backend renders into a
    /// query and the predicate evaluated in memory cannot disagree.
    pub fn admits(&self, object_targets: &[Target]) -> bool {
        self.target_filter().admits(object_targets)
    }

    /// The target predicate, as data a storage backend can render into a query.
    ///
    /// A SQL backend must filter before it paginates, which means the rule has to reach the
    /// `WHERE` clause. Handing the backend *data* rather than asking it to re-derive the rule is
    /// what keeps object privacy to one implementation: there is nothing here for a second backend
    /// to get subtly wrong.
    pub fn target_filter(&self) -> TargetFilter {
        // A named target is a filter for everyone, business logic included: `[Def §Response
        // Filtering]` says targeting criteria "include only those objects that include target
        // terms found in the query", and that filters are additive. An untargeted object is
        // ungated, but it does not carry the term either, so it does not match.
        if !self.requested.is_empty() {
            return TargetFilter::AnyOf(if self.role.is_business_logic() {
                self.requested.clone()
            } else {
                // Narrowed to what was actually granted, so naming an ungranted target reveals
                // nothing. An empty result matches nothing, which is the point.
                let grant = self.role.grant();
                self.requested
                    .iter()
                    .filter(|t| grant.contains(t))
                    .cloned()
                    .collect()
            });
        }

        if self.role.is_business_logic() {
            return TargetFilter::Any;
        }
        match self.empty {
            Empty::HidesTargeted => TargetFilter::Untargeted,
            Empty::MeansEverythingGranted => {
                TargetFilter::UntargetedOrAnyOf(self.role.grant().targets().to_vec())
            }
        }
    }

    /// The plain `?targets=` filter, with no privacy in it.
    ///
    /// `ven` and `resource` objects are gated by *ownership*: `[Def §Object Privacy]` says target
    /// hiding is deliberately not performed on them, "as these objects are read-able only by a
    /// specific VEN". So `?targets=` on those collections is an ordinary additive filter, and
    /// running the privacy rule there as well would hide a VEN's own object from it whenever
    /// business logic had granted it a target.
    pub fn requested_filter(&self) -> TargetFilter {
        if self.requested.is_empty() {
            TargetFilter::Any
        } else {
            TargetFilter::AnyOf(self.requested.clone())
        }
    }

    /// What the caller is effectively asking for: its request, narrowed to its grant.
    fn effective_targets(&self) -> Vec<Target> {
        let grant = self.role.grant();
        if self.requested.is_empty() {
            grant.targets().to_vec()
        } else {
            self.requested
                .iter()
                .filter(|t| grant.contains(t))
                .cloned()
                .collect()
        }
    }

    /// Whether an owned object is visible, as data a backend can render into a query.
    pub fn owner_filter(&self) -> OwnerFilter {
        match &self.role {
            Role::BusinessLogic => OwnerFilter::Any,
            Role::Ven { client_id, .. } => OwnerFilter::Only(client_id.clone()),
            Role::Anonymous => OwnerFilter::Nothing,
        }
    }

    /// The targets to show on a visible object, or `None` if it is not visible.
    ///
    /// The list is the *intersection* of what the reader asked for and what the object carries —
    /// never the object's full set, which would leak the existence of other target groups.
    pub fn visible_targets(&self, object_targets: &[Target]) -> Option<Vec<Target>> {
        // Visibility *is* the filter. Deriving it here rather than restating the rule is what stops
        // the predicate a backend renders into SQL and the one evaluated in memory from drifting.
        if !self.target_filter().admits(object_targets) {
            return None;
        }
        // Business logic sees every target intact.
        if self.role.is_business_logic() {
            return Some(object_targets.to_vec());
        }
        // An object with no targets is not gated by targeting, so there is nothing to hide.
        if object_targets.is_empty() {
            return Some(Vec::new());
        }
        // Show only the effective targets the object actually carries — never its full set, which
        // would tell the reader which other groups exist.
        let mut visible: Vec<Target> = self
            .effective_targets()
            .into_iter()
            .filter(|t| object_targets.contains(t))
            .collect();
        visible.sort();
        visible.dedup();
        Some(visible)
    }

    /// Whether a client-owned object (`ven`, `resource`, `subscription`, `report`) belongs to the
    /// caller.
    pub fn owns(&self, owner: Option<&ClientId>) -> bool {
        self.owner_filter().admits(owner)
    }
}

/// The targeting predicate, reduced to something a query can express.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetFilter {
    /// Everything matches: no targeting gate and no target filter.
    Any,
    /// Only objects carrying no targets at all — a VEN that named none.
    Untargeted,
    /// Objects carrying no targets, or at least one of these.
    ///
    /// The reading of an *unfiltered* request by a VEN: untargeted objects are ungated, and
    /// targeted ones are admitted where the grant reaches. An empty list therefore means the same
    /// as [`TargetFilter::Untargeted`].
    UntargetedOrAnyOf(Vec<Target>),
    /// Only objects carrying at least one of these — a request that named targets.
    ///
    /// An empty list matches nothing, which is what "you asked for a target you were not granted"
    /// has to mean.
    AnyOf(Vec<Target>),
}

impl TargetFilter {
    /// Evaluate in memory. A backend that renders this into SQL must agree with this function.
    pub fn admits(&self, object_targets: &[Target]) -> bool {
        match self {
            TargetFilter::Any => true,
            TargetFilter::Untargeted => object_targets.is_empty(),
            TargetFilter::UntargetedOrAnyOf(allowed) => {
                object_targets.is_empty() || object_targets.iter().any(|t| allowed.contains(t))
            }
            TargetFilter::AnyOf(allowed) => object_targets.iter().any(|t| allowed.contains(t)),
        }
    }

    /// The targets a query should match against, if any.
    pub fn allowed(&self) -> &[Target] {
        match self {
            TargetFilter::UntargetedOrAnyOf(t) | TargetFilter::AnyOf(t) => t,
            _ => &[],
        }
    }
}

/// The ownership predicate, reduced to something a query can express.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerFilter {
    /// Every object, whoever owns it.
    Any,
    /// Only objects owned by this client.
    Only(ClientId),
    /// Nothing: an unauthenticated caller owns no objects.
    Nothing,
}

impl OwnerFilter {
    /// Evaluate in memory.
    pub fn admits(&self, owner: Option<&ClientId>) -> bool {
        match self {
            OwnerFilter::Any => true,
            OwnerFilter::Only(mine) => owner == Some(mine),
            OwnerFilter::Nothing => false,
        }
    }
}

/// Builds grants for many clients at once, from their `ven` and `resource` targets.
#[derive(Debug, Default)]
pub struct GrantIndex {
    by_client: BTreeMap<ClientId, Grant>,
}

impl GrantIndex {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record targets for a client, from either its VEN object or one of its resources.
    pub fn add(&mut self, client_id: &ClientId, targets: impl IntoIterator<Item = Target>) {
        match self.by_client.get_mut(client_id) {
            Some(grant) => grant.extend(targets),
            None => {
                self.by_client
                    .insert(client_id.clone(), Grant::from_targets(targets));
            }
        }
    }

    /// The grant for a client, or an empty one.
    pub fn get(&self, client_id: &ClientId) -> Grant {
        self.by_client.get(client_id).cloned().unwrap_or_default()
    }

    /// Every grant.
    pub fn iter(&self) -> impl Iterator<Item = (&ClientId, &Grant)> {
        self.by_client.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::std_shim::vec;

    fn t(s: &str) -> Target {
        Target::new(s).unwrap()
    }

    fn ven(grants: &[&str]) -> Role {
        Role::Ven {
            client_id: ClientId::new("ven-client").unwrap(),
            grant: Grant::from_targets(grants.iter().map(|s| t(s))),
        }
    }

    #[test]
    fn business_logic_sees_every_target() {
        let a = Access::list(Role::BusinessLogic, vec![]);
        assert_eq!(
            a.visible_targets(&[t("gold"), t("silver")]),
            Some(vec![t("gold"), t("silver")])
        );
    }

    #[test]
    fn a_named_target_filters_for_business_logic_too() {
        // Otherwise `GET /events?targets=x` would return everything to business logic.
        let a = Access::list(Role::BusinessLogic, vec![t("gold")]);
        assert!(a.admits(&[t("gold")]));
        assert!(!a.admits(&[t("silver")]));
        // `[Def §Response Filtering]`: a target term includes "only those objects that include
        // target terms found in the query". An untargeted object carries none, so it is filtered
        // out — it is reachable by simply not naming a target.
        assert!(!a.admits(&[]));
        assert!(Access::list(Role::BusinessLogic, vec![]).admits(&[]));
    }

    #[test]
    fn a_read_by_id_still_sees_an_untargeted_object() {
        // The filtering reading above must not reach the by-id and push paths, where an empty
        // request means "everything I am entitled to" rather than "nothing".
        assert!(Access::by_id(ven(&["gold"])).admits(&[]));
        assert!(Access::push(ven(&["gold"]), vec![]).admits(&[]));
        // A subscription that names targets is asking to be told about those, and only those.
        assert!(!Access::push(ven(&["gold"]), vec![t("gold")]).admits(&[]));
    }

    #[test]
    fn the_requested_filter_carries_no_privacy() {
        // Ownership gates `ven` and `resource`; `?targets=` there is a plain additive filter, and
        // running the grant through it would hide a VEN's own object from itself.
        let a = Access::list(ven(&[]), vec![t("gold")]);
        assert!(a.requested_filter().admits(&[t("gold")]));
        assert!(!a.requested_filter().admits(&[t("silver")]));
        assert!(!a.requested_filter().admits(&[]));
        assert_eq!(
            Access::list(ven(&[]), vec![]).requested_filter(),
            TargetFilter::Any
        );
    }

    #[test]
    fn untargeted_objects_are_public_to_every_reader() {
        assert!(Access::list(ven(&[]), vec![]).admits(&[]));
        assert!(Access::list(Role::Anonymous, vec![]).admits(&[]));
    }

    #[test]
    fn a_ven_must_name_the_targets_it_wants_when_listing() {
        assert!(!Access::list(ven(&["gold"]), vec![]).admits(&[t("gold")]));
        assert!(Access::list(ven(&["gold"]), vec![t("gold")]).admits(&[t("gold")]));
    }

    #[test]
    fn a_read_by_id_does_not_need_named_targets_but_still_needs_the_grant() {
        assert!(Access::by_id(ven(&["gold"])).admits(&[t("gold")]));
        assert!(!Access::by_id(ven(&["silver"])).admits(&[t("gold")]));
        assert!(!Access::by_id(Role::Anonymous).admits(&[t("gold")]));
    }

    #[test]
    fn asking_for_an_ungranted_target_reveals_nothing() {
        assert!(!Access::list(ven(&["gold"]), vec![t("platinum")]).admits(&[t("platinum")]));
    }

    #[test]
    fn target_hiding_never_leaks_the_objects_other_groups() {
        let a = Access::list(ven(&["gold"]), vec![t("gold")]);
        assert_eq!(
            a.visible_targets(&[t("gold"), t("silver"), t("bronze")]),
            Some(vec![t("gold")]),
            "the reader must not learn that silver and bronze exist"
        );
    }

    #[test]
    fn a_partly_granted_request_still_admits_what_was_granted() {
        // The case that broke pagination: entitled to one of two requested targets.
        let a = Access::list(ven(&["gold"]), vec![t("gold"), t("silver")]);
        assert!(a.admits(&[t("gold")]));
        assert!(!a.admits(&[t("silver")]));
        assert_eq!(a.visible_targets(&[t("gold")]), Some(vec![t("gold")]));
    }

    #[test]
    fn push_defaults_an_empty_request_to_the_grant() {
        assert!(Access::push(ven(&["gold"]), vec![]).admits(&[t("gold")]));
        assert!(!Access::push(ven(&["gold"]), vec![]).admits(&[t("silver")]));
    }

    #[test]
    fn ownership_is_by_client_id() {
        let a = Access::list(ven(&[]), vec![]);
        let mine = ClientId::new("ven-client").unwrap();
        let theirs = ClientId::new("other").unwrap();
        assert!(a.owns(Some(&mine)));
        assert!(!a.owns(Some(&theirs)));
        assert!(!a.owns(None));
        assert!(Access::list(Role::BusinessLogic, vec![]).owns(Some(&theirs)));
        assert!(!Access::list(Role::Anonymous, vec![]).owns(Some(&mine)));
    }

    #[test]
    fn the_target_filter_agrees_with_admits() {
        // The filter is what a SQL backend renders; it must mean exactly what `admits` means.
        let cases = [
            Access::list(Role::BusinessLogic, vec![]),
            Access::list(Role::BusinessLogic, vec![t("gold")]),
            Access::list(ven(&["gold"]), vec![]),
            Access::list(ven(&["gold"]), vec![t("gold"), t("silver")]),
            Access::by_id(ven(&["gold"])),
            Access::push(ven(&["gold"]), vec![t("silver")]),
            Access::list(Role::Anonymous, vec![t("gold")]),
        ];
        let objects = [
            vec![],
            vec![t("gold")],
            vec![t("silver")],
            vec![t("gold"), t("silver")],
        ];
        for access in &cases {
            for object in &objects {
                assert_eq!(
                    access.target_filter().admits(object),
                    access.visible_targets(object).is_some(),
                    "filter and visibility disagree for {access:?} on {object:?}"
                );
            }
        }
    }

    #[test]
    fn the_owner_filter_agrees_with_owns() {
        let mine = ClientId::new("ven-client").unwrap();
        let theirs = ClientId::new("other").unwrap();
        for access in [
            Access::list(Role::BusinessLogic, vec![]),
            Access::list(ven(&[]), vec![]),
            Access::list(Role::Anonymous, vec![]),
        ] {
            for owner in [None, Some(&mine), Some(&theirs)] {
                assert_eq!(access.owner_filter().admits(owner), access.owns(owner));
            }
        }
    }

    #[test]
    fn a_grant_is_the_union_of_ven_and_resource_targets() {
        let mut index = GrantIndex::new();
        let c = ClientId::new("c1").unwrap();
        index.add(&c, [t("ven_0999"), t("group1")]);
        index.add(&c, [t("areaX"), t("group1")]);
        assert_eq!(
            index.get(&c).targets(),
            &[t("areaX"), t("group1"), t("ven_0999")]
        );
    }
}
