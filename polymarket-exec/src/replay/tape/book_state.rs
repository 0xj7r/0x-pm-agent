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
        self.refresh_top(event.leg);
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
            (LEG_YES, SIDE_BID) => self.yes_bids[index] = size,
            (LEG_YES, SIDE_ASK) => self.yes_asks[index] = size,
            (LEG_NO, SIDE_BID) => self.no_bids[index] = size,
            (LEG_NO, SIDE_ASK) => self.no_asks[index] = size,
            _ => {}
        }
    }

    fn refresh_top(&mut self, leg: u8) {
        match leg {
            LEG_YES => {
                self.yes_top = top_from_levels(&self.yes_bids, &self.yes_asks);
            }
            LEG_NO => {
                self.no_top = top_from_levels(&self.no_bids, &self.no_asks);
            }
            _ => {}
        }
    }
}

fn price_index(price_ticks: u32) -> Option<usize> {
    let index = price_ticks as usize;
    (index <= MAX_PRICE_TICKS).then_some(index)
}

fn top_from_levels(bids: &[u32; PRICE_LEVELS], asks: &[u32; PRICE_LEVELS]) -> TopOfBook {
    let bid_price = bids.iter().rposition(|size| *size > 0).map(|idx| idx as u32);
    let ask_price = asks.iter().position(|size| *size > 0).map(|idx| idx as u32);

    TopOfBook {
        bid_price_ticks: bid_price,
        bid_size_lots: bid_price.map(|idx| bids[idx as usize]).unwrap_or(0),
        ask_price_ticks: ask_price,
        ask_size_lots: ask_price.map(|idx| asks[idx as usize]).unwrap_or(0),
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
}

