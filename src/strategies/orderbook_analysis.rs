#[derive(Debug, Clone)]
pub struct OrderBook {
    pub bids: Vec<(f64, f64)>,
    pub asks: Vec<(f64, f64)>,
}

#[derive(Debug, Clone)]
pub struct Density {
    pub price: f64,
    pub volume: f64,
}

pub struct OrderBookAnalyzer {
    pub density_threshold: f64,
}

impl OrderBookAnalyzer {
    pub fn new(threshold: f64) -> Self {
        Self { density_threshold: threshold }
    }

    pub fn analyze_densities(&self, ob: &OrderBook) -> Vec<Density> {
        ob.bids
            .iter()
            .chain(ob.asks.iter())
            .filter(|(_, v)| *v > self.density_threshold)
            .map(|(p, v)| Density { price: *p, volume: *v })
            .collect()
    }

    pub fn nearest_density(&self, ob: &OrderBook, current_price: f64) -> Option<Density> {
        self.analyze_densities(ob)
            .into_iter()
            .min_by(|a, b| {
                (a.price - current_price).abs().partial_cmp(&(b.price - current_price).abs()).unwrap()
            })
    }
}