use std::fmt::{self, Debug};

use tracing::{error, trace, warn};

use super::{
    endorsement::{Endorsement, SignedEndorsement},
    evidence::Evidence,
    highway::{Endorsements, ValidVertex, Vertex, WireUnit},
    state::{self, Panorama, State, Unit},
    validators::ValidatorIndex,
};

use crate::{
    components::consensus::{
        consensus_protocol::BlockContext,
        highway_core::highway::SignedWireUnit,
        traits::{Context, ValidatorSecret},
    },
    types::{TimeDiff, Timestamp},
    NodeRng,
};

/// An action taken by a validator.
#[derive(Clone, Eq, PartialEq, Debug)]
pub(crate) enum Effect<C: Context> {
    /// Newly vertex that should be gossiped to peers and added to the protocol state.
    NewVertex(ValidVertex<C>),
    /// `handle_timer` needs to be called at the specified time.
    ScheduleTimer(Timestamp),
    /// `propose` needs to be called with a value for a new block with the specified block context
    /// and parent value.
    RequestNewBlock(BlockContext),
    /// This validator produced an equivocation.
    ///
    /// When this is returned, the validator automatically deactivates.
    WeEquivocated(Evidence<C>),
}

/// A validator that actively participates in consensus by creating new vertices.
///
/// It implements the Highway schedule. The protocol proceeds in rounds, and in each round one
/// validator is the _leader_.
/// * In the beginning of the round, the leader sends a _proposal_ unit, containing a consensus
///   value (i.e. a block).
/// * Upon receiving the proposal, all the other validators send a _confirmation_ unit, citing only
///   the proposal, their own previous message, and resulting transitive justifications.
/// * At a fixed point in time later in the round, everyone unconditionally sends a _witness_ unit,
///   citing every unit they have received so far.
///
/// If the rounds are long enough (i.e. message delivery is fast enough) and there are enough
/// honest validators, there will be a lot of confirmations for the proposal, and enough witness
/// units citing all those confirmations, to create a summit and finalize the proposal.
pub(crate) struct ActiveValidator<C: Context> {
    /// Our own validator index.
    vidx: ValidatorIndex,
    /// The validator's secret signing key.
    secret: C::ValidatorSecret,
    /// The next round exponent: Our next round will be `1 << next_round_exp` milliseconds long.
    next_round_exp: u8,
    /// The latest timer we scheduled.
    next_timer: Timestamp,
    /// Panorama and timestamp for a block we are about to propose when we get a consensus value.
    next_proposal: Option<(Timestamp, Panorama<C>)>,
}

impl<C: Context> Debug for ActiveValidator<C> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ActiveValidator")
            .field("vidx", &self.vidx)
            .field("next_round_exp", &self.next_round_exp)
            .field("next_timer", &self.next_timer)
            .finish()
    }
}

impl<C: Context> ActiveValidator<C> {
    /// Creates a new `ActiveValidator` and the timer effect for the first call.
    pub(crate) fn new(
        vidx: ValidatorIndex,
        secret: C::ValidatorSecret,
        start_time: Timestamp,
        state: &State<C>,
    ) -> (Self, Vec<Effect<C>>) {
        let mut av = ActiveValidator {
            vidx,
            secret,
            next_round_exp: state.params().init_round_exp(),
            next_timer: Timestamp::zero(),
            next_proposal: None,
        };
        let effects = av.schedule_timer(start_time, state);
        (av, effects)
    }

    /// Sets the next round exponent to the new value.
    pub(crate) fn set_round_exp(&mut self, new_round_exp: u8) {
        self.next_round_exp = new_round_exp;
    }

    /// Returns actions a validator needs to take at the specified `timestamp`, with the given
    /// protocol `state`.
    pub(crate) fn handle_timer(
        &mut self,
        timestamp: Timestamp,
        state: &State<C>,
        instance_id: C::InstanceId,
        rng: &mut NodeRng,
    ) -> Vec<Effect<C>> {
        if self.is_faulty(state) {
            warn!("Creator knows it's faulty. Won't create a message.");
            return vec![];
        }
        let mut effects = self.schedule_timer(timestamp, state);
        if self.earliest_unit_time(state) > timestamp {
            warn!(%timestamp, "skipping outdated timer event");
            return effects;
        }
        let r_exp = self.round_exp(state, timestamp);
        let r_id = state::round_id(timestamp, r_exp);
        let r_len = state::round_len(r_exp);
        if timestamp == r_id && state.leader(r_id) == self.vidx {
            effects.extend(self.request_new_block(state, instance_id, timestamp, rng))
        } else if timestamp == r_id + self.witness_offset(r_len) {
            let panorama = state.citable_panorama().cutoff(state, timestamp);
            if panorama.has_correct() {
                let witness_unit =
                    self.new_unit(panorama, timestamp, None, state, instance_id, rng);
                effects.push(Effect::NewVertex(ValidVertex(Vertex::Unit(witness_unit))))
            }
        }
        effects
    }

    /// Returns actions a validator needs to take upon receiving a new unit.
    pub(crate) fn on_new_unit(
        &mut self,
        vhash: &C::Hash,
        now: Timestamp,
        state: &State<C>,
        instance_id: C::InstanceId,
        rng: &mut NodeRng,
    ) -> Vec<Effect<C>> {
        if let Some(evidence) = state.opt_evidence(self.vidx) {
            return vec![Effect::WeEquivocated(evidence.clone())];
        }
        let mut effects = vec![];
        if self.should_send_confirmation(vhash, now, state) {
            let panorama = state.confirmation_panorama(self.vidx, vhash);
            if panorama.has_correct() {
                let confirmation_unit = self.new_unit(panorama, now, None, state, instance_id, rng);
                let vv = ValidVertex(Vertex::Unit(confirmation_unit));
                effects.extend(vec![Effect::NewVertex(vv)])
            }
        };
        if self.should_endorse(vhash, state) {
            let endorsement = self.endorse(vhash, rng);
            effects.extend(vec![Effect::NewVertex(ValidVertex(endorsement))]);
        }
        effects
    }

    /// Returns actions validator needs to take upon receiving a new evidence.
    /// Endorses all latest units by honest validators that do not mark new perpetrator as faulty
    /// and cite some new message by that validator.
    pub(crate) fn on_new_evidence(
        &mut self,
        evidence: &Evidence<C>,
        state: &State<C>,
        rng: &mut NodeRng,
    ) -> Vec<Effect<C>> {
        let vidx = evidence.perpetrator();
        state
            .iter_correct_hashes()
            .filter(|&v| {
                let unit = state.unit(v);
                unit.new_hash_obs(state, vidx)
            })
            .map(|v| self.endorse(v, rng))
            .map(|endorsement| Effect::NewVertex(ValidVertex(endorsement)))
            .collect()
    }

    /// Returns an effect to request a consensus value for a block to propose.
    ///
    /// If we are already waiting for a consensus value, `None` is returned instead.
    /// If the new value would come after a terminal block, the proposal is made immediately, and
    /// without a value.
    pub(crate) fn request_new_block(
        &mut self,
        state: &State<C>,
        instance_id: C::InstanceId,
        timestamp: Timestamp,
        rng: &mut NodeRng,
    ) -> Option<Effect<C>> {
        if let Some((prop_time, _)) = self.next_proposal {
            warn!(
                ?timestamp,
                "skipping proposal, still waiting for value for {}", prop_time
            );
            return None;
        }
        let panorama = state.citable_panorama().cutoff(state, timestamp);
        let opt_parent_hash = state.fork_choice(&panorama);
        if opt_parent_hash.map_or(false, |hash| state.is_terminal_block(hash)) {
            let proposal_unit = self.new_unit(panorama, timestamp, None, state, instance_id, rng);
            return Some(Effect::NewVertex(ValidVertex(Vertex::Unit(proposal_unit))));
        }
        let opt_parent = opt_parent_hash.map(|bh| state.block(bh));
        let height = opt_parent.map_or(0, |block| block.height);
        self.next_proposal = Some((timestamp, panorama));
        let bctx = BlockContext::new(timestamp, height);
        Some(Effect::RequestNewBlock(bctx))
    }

    /// Proposes a new block with the given consensus value.
    pub(crate) fn propose(
        &mut self,
        value: C::ConsensusValue,
        block_context: BlockContext,
        state: &State<C>,
        instance_id: C::InstanceId,
        rng: &mut NodeRng,
    ) -> Vec<Effect<C>> {
        let timestamp = block_context.timestamp();
        if self.earliest_unit_time(state) > timestamp {
            warn!(?block_context, "skipping outdated proposal");
            return vec![];
        }
        if self.is_faulty(state) {
            warn!("Creator knows it's faulty. Won't create a message.");
            return vec![];
        }
        let panorama = if let Some((prop_time, panorama)) = self.next_proposal.take() {
            if prop_time != timestamp {
                warn!(
                    ?timestamp,
                    "unexpected proposal; expected timestamp {}", prop_time
                );
                return vec![];
            }
            panorama
        } else {
            warn!("unexpected proposal value");
            return vec![];
        };
        let proposal_unit =
            self.new_unit(panorama, timestamp, Some(value), state, instance_id, rng);
        vec![Effect::NewVertex(ValidVertex(Vertex::Unit(proposal_unit)))]
    }

    /// Returns whether the incoming message is a proposal that we need to send a confirmation for.
    fn should_send_confirmation(
        &self,
        vhash: &C::Hash,
        timestamp: Timestamp,
        state: &State<C>,
    ) -> bool {
        let earliest_unit_time = self.earliest_unit_time(state);
        if timestamp < earliest_unit_time {
            warn!(
                %earliest_unit_time, %timestamp,
                "earliest_unit_time is greater than current time stamp"
            );
            return false;
        }
        let unit = state.unit(vhash);
        if unit.timestamp > timestamp {
            error!(
                %unit.timestamp, %timestamp,
                "added a unit with a future timestamp, should never happen"
            );
            return false;
        }
        // If it's not a proposal, the sender is faulty, or we are, don't send a confirmation.
        if unit.creator == self.vidx || self.is_faulty(state) || !state.is_correct_proposal(unit) {
            return false;
        }
        if let Some(unit) = self.latest_unit(state) {
            if unit.panorama.sees_correct(state, vhash) {
                error!(%vhash, "called on_new_unit with already confirmed proposal");
                return false; // We already sent a confirmation.
            }
        }
        let r_id = state::round_id(timestamp, self.round_exp(state, timestamp));
        if unit.timestamp != r_id {
            trace!(
                %unit.timestamp, %r_id,
                "not confirming proposal: wrong round",
            );
            return false;
        }
        true
    }

    /// Returns a new unit with the given data, and the correct sequence number.
    fn new_unit(
        &mut self,
        mut panorama: Panorama<C>,
        timestamp: Timestamp,
        value: Option<C::ConsensusValue>,
        state: &State<C>,
        instance_id: C::InstanceId,
        rng: &mut NodeRng,
    ) -> SignedWireUnit<C> {
        if let Some((prop_time, _)) = self.next_proposal.take() {
            warn!(
                ?timestamp,
                "canceling proposal for {} due to unit", prop_time
            );
        }
        if panorama[self.vidx] != state.panorama()[self.vidx] {
            error!("replacing unit panorama to avoid equivocation");
            panorama = state.citable_panorama().clone();
        }
        let seq_number = panorama.next_seq_num(state, self.vidx);
        let endorsed = state.seen_endorsed(&panorama);
        let wunit = WireUnit {
            panorama,
            creator: self.vidx,
            instance_id,
            value,
            seq_number,
            timestamp,
            round_exp: self.round_exp(state, timestamp),
            endorsed,
        };
        SignedWireUnit::new(wunit, &self.secret, rng)
    }

    /// Returns a `ScheduleTimer` effect for the next time we need to be called.
    ///
    /// If the time is before the current round's witness unit, schedule the witness unit.
    /// Otherwise, if we are the next round's leader, schedule the proposal unit.
    /// Otherwise schedule the next round's witness unit.
    fn schedule_timer(&mut self, timestamp: Timestamp, state: &State<C>) -> Vec<Effect<C>> {
        if self.next_timer > timestamp {
            return Vec::new(); // We already scheduled the next call; nothing to do.
        }
        let r_exp = self.round_exp(state, timestamp);
        let r_id = state::round_id(timestamp, r_exp);
        let r_len = state::round_len(r_exp);
        self.next_timer = if timestamp < r_id + self.witness_offset(r_len) {
            r_id + self.witness_offset(r_len)
        } else {
            let next_r_id = r_id + r_len;
            if state.leader(next_r_id) == self.vidx {
                next_r_id
            } else {
                let next_r_exp = self.round_exp(state, next_r_id);
                next_r_id + self.witness_offset(state::round_len(next_r_exp))
            }
        };
        vec![Effect::ScheduleTimer(self.next_timer)]
    }

    /// Returns the earliest timestamp where we can cast our next unit: It can't be earlier than
    /// our previous unit, and it can't be the third unit in a single round.
    fn earliest_unit_time(&self, state: &State<C>) -> Timestamp {
        self.latest_unit(state)
            .map_or_else(Timestamp::zero, |unit| {
                unit.previous().map_or(unit.timestamp, |vh2| {
                    let unit2 = state.unit(vh2);
                    unit.timestamp.max(unit2.round_id() + unit2.round_len())
                })
            })
    }

    /// Returns the most recent unit by this validator.
    fn latest_unit<'a>(&self, state: &'a State<C>) -> Option<&'a Unit<C>> {
        state
            .panorama()
            .get(self.vidx)
            .correct()
            .map(|vh| state.unit(vh))
    }

    /// Checks if validator knows it's faulty.
    fn is_faulty(&self, state: &State<C>) -> bool {
        state.panorama().get(self.vidx).is_faulty()
    }

    /// Returns the duration after the beginning of a round when the witness units are sent.
    fn witness_offset(&self, round_len: TimeDiff) -> TimeDiff {
        round_len * 2 / 3
    }

    /// The round exponent of the round containing `timestamp`.
    ///
    /// This returns `self.next_round_exp`, if that is a valid round exponent for a unit cast at
    /// `timestamp`. Otherwise it returns the round exponent of our latest unit.
    fn round_exp(&self, state: &State<C>, timestamp: Timestamp) -> u8 {
        self.latest_unit(state).map_or(self.next_round_exp, |unit| {
            let max_re = self.next_round_exp.max(unit.round_exp);
            if unit.timestamp < state::round_id(timestamp, max_re) {
                self.next_round_exp
            } else {
                unit.round_exp
            }
        })
    }

    /// Returns whether we should endorse the `vhash`.
    ///
    /// We should endorse unit from honest validator that cites _an_ equivocator
    /// as honest and it cites some new message by that validator.
    fn should_endorse(&self, vhash: &C::Hash, state: &State<C>) -> bool {
        let unit = state.unit(vhash);
        !state.is_faulty(unit.creator)
            && unit
                .panorama
                .enumerate()
                .any(|(vidx, _)| state.is_faulty(vidx) && unit.new_hash_obs(state, vidx))
    }

    /// Creates endorsement of the `vhash`.
    fn endorse(&self, vhash: &C::Hash, rng: &mut NodeRng) -> Vertex<C> {
        let endorsement = Endorsement::new(*vhash, self.vidx);
        let signature = self.secret.sign(&endorsement.hash(), rng);
        Vertex::Endorsements(Endorsements::new(vec![SignedEndorsement::new(
            endorsement,
            signature,
        )]))
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;

    use super::{
        super::{
            finality_detector::FinalityDetector,
            state::{tests::*, Weight},
        },
        Vertex, *,
    };

    type Eff = Effect<TestContext>;

    impl Eff {
        fn unwrap_unit(self) -> SignedWireUnit<TestContext> {
            if let Eff::NewVertex(ValidVertex(Vertex::Unit(swunit))) = self {
                swunit
            } else {
                panic!("Unexpected effect: {:?}", self);
            }
        }
    }

    fn unwrap_single<T: Debug>(vec: Vec<T>) -> T {
        let mut iter = vec.into_iter();
        match (iter.next(), iter.next()) {
            (None, _) => panic!("Unexpected empty vec"),
            (Some(t), None) => t,
            (Some(t0), Some(t1)) => panic!("Expected only one element: {:?}, {:?}", t0, t1),
        }
    }

    #[test]
    #[allow(clippy::unreadable_literal)] // 0xC0FFEE is more readable than 0x00C0_FFEE.
    fn active_validator() -> Result<(), AddUnitError<TestContext>> {
        let mut state = State::new_test(&[Weight(3), Weight(4)], 0);
        let mut rng = crate::new_rng();
        let mut fd = FinalityDetector::new(Weight(2));
        let instance_id = 1u64;

        // We start at time 410, with round length 16, so the first leader tick is 416, and the
        // first witness tick 426.
        assert_eq!(ALICE, state.leader(416.into())); // Alice will be the first leader.
        assert_eq!(BOB, state.leader(432.into())); // Bob will be the second leader.
        let (mut alice_av, effects) =
            ActiveValidator::new(ALICE, TestSecret(0), 410.into(), &state);
        assert_eq!([Eff::ScheduleTimer(416.into())], *effects);
        let (mut bob_av, effects) = ActiveValidator::new(BOB, TestSecret(1), 410.into(), &state);
        assert_eq!([Eff::ScheduleTimer(426.into())], *effects);

        assert!(alice_av
            .handle_timer(415.into(), &state, instance_id, &mut rng)
            .is_empty()); // Too early: No new effects.

        // Alice wants to propose a block, and also make her witness unit at 426.
        let bctx = match &*alice_av.handle_timer(416.into(), &state, instance_id, &mut rng) {
            [Eff::ScheduleTimer(timestamp), Eff::RequestNewBlock(bctx)]
                if *timestamp == 426.into() =>
            {
                bctx.clone()
            }
            effects => panic!("unexpected effects {:?}", effects),
        };
        assert_eq!(Timestamp::from(416), bctx.timestamp());

        // She has a pending deploy from Colin who wants to pay for a hot beverage.
        let effects = alice_av.propose(0xC0FFEE, bctx, &state, instance_id, &mut rng);
        let proposal_wunit = unwrap_single(effects).unwrap_unit();
        let prop_hash = proposal_wunit.hash();
        state.add_unit(proposal_wunit)?;
        assert!(alice_av
            .on_new_unit(&prop_hash, 417.into(), &state, instance_id, &mut rng)
            .is_empty());

        // Bob creates a confirmation unit for Alice's proposal.
        let effects = bob_av.on_new_unit(&prop_hash, 419.into(), &state, instance_id, &mut rng);
        state.add_unit(unwrap_single(effects).unwrap_unit())?;

        // Bob creates his witness message 2/3 through the round.
        let mut effects = bob_av
            .handle_timer(426.into(), &state, instance_id, &mut rng)
            .into_iter();
        assert_eq!(Some(Eff::ScheduleTimer(432.into())), effects.next()); // Bob is the next leader.
        state.add_unit(effects.next().unwrap().unwrap_unit())?;
        assert_eq!(None, effects.next());

        // Alice has not witnessed Bob's unit yet.
        assert_eq!(None, fd.next_finalized(&state));

        // Alice also sends her own witness message, completing the summit for her proposal.
        let mut effects = alice_av
            .handle_timer(426.into(), &state, instance_id, &mut rng)
            .into_iter();
        assert_eq!(Some(Eff::ScheduleTimer(442.into())), effects.next()); // Timer for witness unit.
        state.add_unit(effects.next().unwrap().unwrap_unit())?;
        assert_eq!(None, effects.next());

        // Payment finalized! "One Pumpkin Spice Mochaccino for Corbyn!"
        assert_eq!(Some(&prop_hash), fd.next_finalized(&state));
        Ok(())
    }
}
