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

/// A routing group over `n` single-model providers, weighted as given.
fn group_table(primary: &[f64], fallback: &[f64]) -> RoutingTable {
    let mut toml = String::new();
    for i in 0..primary.len() + fallback.len() {
        toml.push_str(&format!(
            "[providers.p{i}]\ntype = \"openai_compat\"\nbase_url = \"http://127.0.0.1:1/v1\"\n\
             keys = [{{ name = \"k\", value = \"sk\", models = [\"m\"] }}]\n"
        ));
    }
    let pool = |offset: usize, weights: &[f64]| {
        weights
            .iter()
            .enumerate()
            .map(|(i, w)| format!("{{ target = \"p{}/m\", weight = {w} }}", i + offset))
            .collect::<Vec<_>>()
            .join(", ")
    };
    toml.push_str(&format!(
        "[groups.fast]\nprimary = [{}]\n",
        pool(0, primary)
    ));
    if !fallback.is_empty() {
        toml.push_str(&format!("fallback = [{}]\n", pool(primary.len(), fallback)));
    }
    let config = Config::from_str_with_env(&toml, Format::Toml, &|_: &str| None).unwrap();
    RoutingTable::from_config(&config)
}

/// The provider index of the nth target in a plan.
fn nth_provider(table: &RoutingTable, n: usize) -> usize {
    let plan = table.plan("fast").unwrap();
    plan.targets[n].provider.name[1..].parse().unwrap()
}

proptest! {
    /// The target a request actually goes to — the head of the plan — is
    /// drawn in proportion to its primary weight. This is the traffic
    /// split an operator configures a group for.
    #[test]
    fn group_traffic_follows_primary_weights(
        weights in prop::collection::vec(0.05f64..10.0, 2..5),
        seed in any::<u64>(),
    ) {
        fastrand::seed(seed);
        let table = group_table(&weights, &[]);

        let draws = 4000usize;
        let mut counts = vec![0usize; weights.len()];
        for _ in 0..draws {
            counts[nth_provider(&table, 0)] += 1;
        }

        let total: f64 = weights.iter().sum();
        for (i, w) in weights.iter().enumerate() {
            let expected = w / total;
            let got = counts[i] as f64 / draws as f64;
            prop_assert!(
                (got - expected).abs() < 0.08,
                "target {i}: expected ~{expected:.3}, got {got:.3} (weights {weights:?})"
            );
        }
    }

    /// The plan covers the whole group once, primary pool first: a
    /// request may exhaust every primary model before the reserve is
    /// touched, and no target is offered twice.
    #[test]
    fn group_plan_exhausts_primary_before_fallback(
        primary in prop::collection::vec(0.05f64..10.0, 1..4),
        fallback in prop::collection::vec(0.05f64..10.0, 1..4),
        seed in any::<u64>(),
    ) {
        fastrand::seed(seed);
        let table = group_table(&primary, &fallback);
        let plan = table.plan("fast").unwrap();

        prop_assert_eq!(plan.targets.len(), primary.len() + fallback.len());
        let order: Vec<usize> = plan
            .targets
            .iter()
            .map(|t| t.provider.name[1..].parse().unwrap())
            .collect();
        let mut seen = order.clone();
        seen.sort_unstable();
        seen.dedup();
        prop_assert_eq!(seen.len(), order.len(), "a target was planned twice");
        prop_assert!(
            order[..primary.len()].iter().all(|&i| i < primary.len()),
            "fallback reached before the primary pool was exhausted: {:?}",
            order
        );
    }
}
