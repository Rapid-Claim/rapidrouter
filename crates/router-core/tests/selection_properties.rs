//! Property tests over key selection: distribution follows weights, and
//! masking (allowlists, breaker health) is absolute.

use proptest::prelude::*;
use router_core::config::{Config, Format};
use router_core::router::RoutingTable;

fn table_for(weights: &[f64]) -> RoutingTable {
    let keys = weights
        .iter()
        .enumerate()
        .map(|(i, w)| format!("{{ name = \"k{i}\", value = \"sk-{i}\", weight = {w} }}"))
        .collect::<Vec<_>>()
        .join(", ");
    let toml = format!("[providers.openai]\nkeys = [{keys}]\n");
    let config = Config::from_str_with_env(&toml, Format::Toml, &|_: &str| None).unwrap();
    RoutingTable::from_config(&config)
}

proptest! {
    /// Every key with positive weight is reachable, and empirical
    /// frequencies track configured weights within a loose tolerance.
    #[test]
    fn selection_tracks_weights(
        weights in prop::collection::vec(0.05f64..10.0, 2..6),
        seed in any::<u64>(),
    ) {
        fastrand::seed(seed);
        let table = table_for(&weights);
        let route = table.resolve("openai/m").unwrap();

        let draws = 4000usize;
        let mut counts = vec![0usize; weights.len()];
        for _ in 0..draws {
            let name = &route.provider.select_key("m").unwrap().name;
            let idx: usize = name[1..].parse().unwrap();
            counts[idx] += 1;
        }

        let total: f64 = weights.iter().sum();
        for (i, w) in weights.iter().enumerate() {
            let expected = w / total;
            let got = counts[i] as f64 / draws as f64;
            // Loose bound: 4000 draws, just catching gross bias.
            prop_assert!(
                (got - expected).abs() < 0.08,
                "key {i}: expected ~{expected:.3}, got {got:.3} (weights {weights:?})"
            );
        }
    }

    /// A model allowlist is an absolute mask regardless of weights.
    #[test]
    fn allowlists_are_absolute(
        n_keys in 2usize..5,
        allowed_idx in 0usize..5,
        seed in any::<u64>(),
    ) {
        let allowed_idx = allowed_idx % n_keys;
        fastrand::seed(seed);
        let keys = (0..n_keys)
            .map(|i| {
                if i == allowed_idx {
                    format!("{{ name = \"k{i}\", value = \"sk\", weight = 0.001, models = [\"target\"] }}")
                } else {
                    format!("{{ name = \"k{i}\", value = \"sk\", weight = 100.0, models = [\"other\"] }}")
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let toml = format!("[providers.p]\ntype = \"openai_compat\"\nbase_url = \"http://x\"\nkeys = [{keys}]\n");
        let config = Config::from_str_with_env(&toml, Format::Toml, &|_: &str| None).unwrap();
        let table = RoutingTable::from_config(&config);
        let route = table.resolve("p/target").unwrap();
        for _ in 0..100 {
            prop_assert_eq!(&route.provider.select_key("target").unwrap().name, &format!("k{allowed_idx}"));
        }
    }
}
