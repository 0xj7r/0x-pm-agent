use crate::replay::tape::format::{
    BookEventV1, EVENT_DELETE, EVENT_SNAPSHOT_END, EVENT_SNAPSHOT_START, EVENT_UPDATE, LEG_NO,
    LEG_YES, SIDE_ASK, SIDE_BID,
};

const MAX_PRICE_TICKS: usize = 10_000;
const PRICE_LEVELS: usize = MAX_PRICE_TICKS + 1;

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct TopOfBook {
    pub bid_price_ticks: Option<u32>,
    pub bid_size_lots: u32,
    pub ask_price_ticks: Option<u32>,
    pub ask_size_lots: u32,
}

#[derive(Debug, Clone)]
pub struct BookState {
    yes_bids: Box<[u32; PRICE_LEVELS]>,
    yes_asks: Box<[u32; PRICE_LEVELS]>,
    no_bids: Box<[u32; PRICE_LEVELS]>,
    no_asks: Box<[u32; PRICE_LEVELS]>,
    yes_top: TopOfBook,
    no_top: TopOfBook,
}

impl Default for BookState {
    fn default() -> Self {
        Self::new()
    }
}

impl BookState {
    pub fn new() -> Self {
        Self {
            yes_bids: Box::new([0; PRICE_LEVELS]),
            yes_asks: Box::new([0; PRICE_LEVELS]),
            no_bids: Box::new([0; PRICE_LEVELS]),
            no_asks: Box::new([0; PRICE_LEVELS]),
            yes_top: TopOfBook::default(),
            no_top: TopOfBook::default(),
        }
    }

    /// Applies a book event and returns true when either best bid/ask changed.
    pub fn apply(&mut self, event: &BookEventV1) -> bool {
        if event.event_type == EVENT_SNAPSHOT_START || event.event_type == EVENT_SNAPSHOT_END {
            return false;
        }

        let previous = self.top(event.leg);
        self.apply_level(event);
        previous != self.top(event.leg)
    }

    pub fn top(&self, leg: u8) -> TopOfBook {
        match leg {
            LEG_YES => self.yes_top,
            LEG_NO => self.no_top,
            _ => TopOfBook::default(),
        }
    }

    fn apply_level(&mut self, event: &BookEventV1) {
        let Some(index) = price_index(event.price_ticks) else {
            return;
        };
        let size = if event.event_type == EVENT_DELETE {
            0
        } else if event.event_type == EVENT_UPDATE {
            event.size_lots
        } else {
            return;
        };

        match (event.leg, event.side) {
            (LEG_YES, SIDE_BID) => {
                apply_side_level(&mut self.yes_bids, &mut self.yes_top, index, size, true);
            }
            (LEG_YES, SIDE_ASK) => {
                apply_side_level(&mut self.yes_asks, &mut self.yes_top, index, size, false);
            }
            (LEG_NO, SIDE_BID) => {
                apply_side_level(&mut self.no_bids, &mut self.no_top, index, size, true);
            }
            (LEG_NO, SIDE_ASK) => {
                apply_side_level(&mut self.no_asks, &mut self.no_top, index, size, false);
            }
            _ => {}
        }
    }
}

fn price_index(price_ticks: u32) -> Option<usize> {
    let index = price_ticks as usize;
    (index <= MAX_PRICE_TICKS).then_some(index)
}

fn apply_side_level(
    levels: &mut [u32; PRICE_LEVELS],
    top: &mut TopOfBook,
    index: usize,
    size: u32,
    is_bid: bool,
) {
    levels[index] = size;

    if is_bid {
        apply_best_bid_level(levels, top, index, size);
    } else {
        apply_best_ask_level(levels, top, index, size);
    }
}

fn apply_best_bid_level(
    levels: &[u32; PRICE_LEVELS],
    top: &mut TopOfBook,
    index: usize,
    size: u32,
) {
    let price = index as u32;
    if size > 0 {
        if top.bid_price_ticks.map_or(true, |best| price >= best) {
            top.bid_price_ticks = Some(price);
            top.bid_size_lots = size;
        }
        return;
    }

    if top.bid_price_ticks == Some(price) {
        if let Some(next_index) = levels[..index]
            .iter()
            .rposition(|level_size| *level_size > 0)
        {
            top.bid_price_ticks = Some(next_index as u32);
            top.bid_size_lots = levels[next_index];
        } else {
            top.bid_price_ticks = None;
            top.bid_size_lots = 0;
        }
    }
}

fn apply_best_ask_level(
    levels: &[u32; PRICE_LEVELS],
    top: &mut TopOfBook,
    index: usize,
    size: u32,
) {
    let price = index as u32;
    if size > 0 {
        if top.ask_price_ticks.map_or(true, |best| price <= best) {
            top.ask_price_ticks = Some(price);
            top.ask_size_lots = size;
        }
        return;
    }

    if top.ask_price_ticks == Some(price) {
        if let Some(offset) = levels[index + 1..]
            .iter()
            .position(|level_size| *level_size > 0)
        {
            let next_index = index + 1 + offset;
            top.ask_price_ticks = Some(next_index as u32);
            top.ask_size_lots = levels[next_index];
        } else {
            top.ask_price_ticks = None;
            top.ask_size_lots = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_top_of_book_for_each_leg() {
        let mut state = BookState::new();
        assert!(state.apply(&BookEventV1 {
            ts_ns: 1,
            price_ticks: 5_000,
            size_lots: 100,
            leg: LEG_YES,
            side: SIDE_BID,
            event_type: EVENT_UPDATE,
            _pad: [0; 5],
        }));
        assert!(state.apply(&BookEventV1 {
            ts_ns: 2,
            price_ticks: 5_100,
            size_lots: 50,
            leg: LEG_YES,
            side: SIDE_BID,
            event_type: EVENT_UPDATE,
            _pad: [0; 5],
        }));
        assert_eq!(state.top(LEG_YES).bid_price_ticks, Some(5_100));
        assert_eq!(state.top(LEG_YES).bid_size_lots, 50);

        assert!(state.apply(&BookEventV1 {
            ts_ns: 3,
            price_ticks: 5_100,
            size_lots: 0,
            leg: LEG_YES,
            side: SIDE_BID,
            event_type: EVENT_DELETE,
            _pad: [0; 5],
        }));
        assert_eq!(state.top(LEG_YES).bid_price_ticks, Some(5_000));
    }

    #[test]
    fn updates_best_size_without_rescanning() {
        let mut state = BookState::new();
        assert!(state.apply(&BookEventV1 {
            ts_ns: 1,
            price_ticks: 5_000,
            size_lots: 100,
            leg: LEG_YES,
            side: SIDE_BID,
            event_type: EVENT_UPDATE,
            _pad: [0; 5],
        }));

        assert!(state.apply(&BookEventV1 {
            ts_ns: 2,
            price_ticks: 5_000,
            size_lots: 125,
            leg: LEG_YES,
            side: SIDE_BID,
            event_type: EVENT_UPDATE,
            _pad: [0; 5],
        }));
        assert_eq!(state.top(LEG_YES).bid_price_ticks, Some(5_000));
        assert_eq!(state.top(LEG_YES).bid_size_lots, 125);
    }

    #[test]
    fn delete_current_best_ask_promotes_next_best() {
        let mut state = BookState::new();
        assert!(state.apply(&BookEventV1 {
            ts_ns: 1,
            price_ticks: 5_100,
            size_lots: 100,
            leg: LEG_NO,
            side: SIDE_ASK,
            event_type: EVENT_UPDATE,
            _pad: [0; 5],
        }));
        assert!(state.apply(&BookEventV1 {
            ts_ns: 2,
            price_ticks: 5_000,
            size_lots: 50,
            leg: LEG_NO,
            side: SIDE_ASK,
            event_type: EVENT_UPDATE,
            _pad: [0; 5],
        }));
        assert_eq!(state.top(LEG_NO).ask_price_ticks, Some(5_000));
        assert_eq!(state.top(LEG_NO).ask_size_lots, 50);

        assert!(state.apply(&BookEventV1 {
            ts_ns: 3,
            price_ticks: 5_000,
            size_lots: 0,
            leg: LEG_NO,
            side: SIDE_ASK,
            event_type: EVENT_DELETE,
            _pad: [0; 5],
        }));
        assert_eq!(state.top(LEG_NO).ask_price_ticks, Some(5_100));
        assert_eq!(state.top(LEG_NO).ask_size_lots, 100);
    }

    #[test]
    fn delete_non_best_level_does_not_change_top() {
        let mut state = BookState::new();
        assert!(state.apply(&BookEventV1 {
            ts_ns: 1,
            price_ticks: 5_000,
            size_lots: 100,
            leg: LEG_YES,
            side: SIDE_BID,
            event_type: EVENT_UPDATE,
            _pad: [0; 5],
        }));
        assert!(!state.apply(&BookEventV1 {
            ts_ns: 2,
            price_ticks: 4_900,
            size_lots: 0,
            leg: LEG_YES,
            side: SIDE_BID,
            event_type: EVENT_DELETE,
            _pad: [0; 5],
        }));
        assert_eq!(state.top(LEG_YES).bid_price_ticks, Some(5_000));
        assert_eq!(state.top(LEG_YES).bid_size_lots, 100);
    }
}
