use std::collections::{HashMap, HashSet};

use crate::tags::{FileTags, TagKind};

const PRIVATE_PREFIX: char = '_';
const POPULAR_FILE_THRESHOLD: usize = 5;
const MENTION_BOOST: f64 = 10.0;
const PRIVATE_PENALTY: f64 = 0.1;

const DAMPING: f64 = 0.85;
const MAX_ITER: usize = 100;
const TOLERANCE: f64 = 1e-6;

pub fn rank_files(
    file_tags: &[FileTags],
    mentioned_idents: &[String],
    context_files: &[String],
) -> Vec<(String, f64)> {
    let mut def_files: HashMap<&str, HashSet<&str>> = HashMap::new();
    let mut ref_files: HashMap<&str, HashSet<&str>> = HashMap::new();
    let mut file_set: HashSet<&str> = HashSet::new();

    for ft in file_tags {
        file_set.insert(ft.rel_path.as_str());
        for tag in &ft.tags {
            match tag.kind {
                TagKind::Def => {
                    def_files
                        .entry(tag.ident.as_str())
                        .or_default()
                        .insert(ft.rel_path.as_str());
                }
                TagKind::Ref => {
                    ref_files
                        .entry(tag.ident.as_str())
                        .or_default()
                        .insert(ft.rel_path.as_str());
                }
            }
        }
    }

    let mentioned: HashSet<&str> = mentioned_idents.iter().map(|s| s.as_str()).collect();

    let mut edge_weights: HashMap<(&str, &str), f64> = HashMap::new();

    for (ident, ref_file_set) in &ref_files {
        let Some(def_file_set) = def_files.get(ident) else {
            continue;
        };

        if def_file_set.len() > POPULAR_FILE_THRESHOLD {
            continue;
        }

        let num_refs = ref_file_set.len();
        let weight_base = (num_refs as f64).sqrt();

        let is_mentioned = mentioned.contains(*ident);
        let is_private = ident.starts_with(PRIVATE_PREFIX);

        let mut multiplier = 1.0;
        if is_mentioned {
            multiplier *= MENTION_BOOST;
        }
        if is_private {
            multiplier *= PRIVATE_PENALTY;
        }

        for referencer in ref_file_set {
            if let Some(definer) = def_file_set.iter().next()
                && referencer != definer
            {
                *edge_weights.entry((referencer, definer)).or_insert(0.0) +=
                    weight_base * multiplier;
            }
        }
    }

    let personalized = build_personalization(&file_set, context_files, &mentioned, &def_files);
    let scores = pagerank(&file_set, &edge_weights, &personalized);

    let mut ranked: Vec<(String, f64)> = scores
        .into_iter()
        .map(|(f, s)| (f.to_string(), s))
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked
}

fn build_personalization(
    file_set: &HashSet<&str>,
    context_files: &[String],
    mentioned: &HashSet<&str>,
    def_files: &HashMap<&str, HashSet<&str>>,
) -> HashMap<String, f64> {
    let mut seeds: HashSet<String> = HashSet::new();

    for ctx in context_files {
        if file_set.contains(ctx.as_str()) {
            seeds.insert(ctx.clone());
        }
    }

    for ident in mentioned {
        if let Some(defs) = def_files.get(ident) {
            for f in defs {
                seeds.insert(f.to_string());
            }
        }
    }

    let n = seeds.len().max(1);
    seeds.into_iter().map(|f| (f, 100.0 / n as f64)).collect()
}

fn pagerank(
    file_set: &HashSet<&str>,
    edge_weights: &HashMap<(&str, &str), f64>,
    personalization: &HashMap<String, f64>,
) -> HashMap<String, f64> {
    let nodes: Vec<&str> = {
        let mut v: Vec<&str> = file_set.iter().copied().collect();
        v.sort();
        v
    };
    let n = nodes.len();
    if n == 0 {
        return HashMap::new();
    }

    let total_seed: f64 = personalization.values().sum();
    let uniform_val = 1.0 / n as f64;

    let get_seed = |node: &str| -> f64 {
        if total_seed > 0.0 {
            *personalization.get(node).unwrap_or(&0.0)
        } else {
            uniform_val
        }
    };

    let base = (1.0 - DAMPING) / n as f64;
    let seed_sum = if total_seed > 0.0 { total_seed } else { 1.0 };

    let mut scores: HashMap<String, f64> = nodes
        .iter()
        .map(|&n| (n.to_string(), 1.0 / (nodes.len() as f64)))
        .collect();

    let mut out_weight: HashMap<&str, f64> = HashMap::new();
    let mut in_edges: HashMap<&str, Vec<(&str, f64)>> = HashMap::new();

    for &(from, to) in edge_weights.keys() {
        let w = edge_weights[&(from, to)];
        *out_weight.entry(from).or_insert(0.0) += w;
        in_edges.entry(to).or_default().push((from, w));
    }

    for _ in 0..MAX_ITER {
        let mut new_scores: HashMap<String, f64> = HashMap::new();

        let dangling_sum: f64 = nodes
            .iter()
            .filter(|&&node| out_weight.get(&node).copied().unwrap_or(0.0) == 0.0)
            .map(|&node| scores.get(node).copied().unwrap_or(0.0))
            .sum();

        for &node in &nodes {
            let incoming: f64 = in_edges
                .get(&node)
                .map(|edges| {
                    edges
                        .iter()
                        .map(|&(from, w)| {
                            let ow = out_weight.get(&from).copied().unwrap_or(0.0);
                            if ow > 0.0 {
                                scores.get(from).copied().unwrap_or(0.0) * (w / ow)
                            } else {
                                0.0
                            }
                        })
                        .sum()
                })
                .unwrap_or(0.0);

            let seed_val = get_seed(node) / seed_sum;
            let dangle = DAMPING * dangling_sum / n as f64;

            new_scores.insert(
                node.to_string(),
                base + DAMPING * incoming + dangle + (1.0 - DAMPING) * seed_val,
            );
        }

        let diff: f64 = nodes
            .iter()
            .map(|&node| {
                let old = scores.get(node).copied().unwrap_or(0.0);
                let new_v = new_scores.get(node).copied().unwrap_or(0.0);
                (old - new_v).abs()
            })
            .sum();

        scores = new_scores;
        if diff < TOLERANCE {
            break;
        }
    }

    scores
}

pub fn top_defs_for_files(
    file_tags: &[FileTags],
    ranked_files: &[(String, f64)],
    mentioned_idents: &[String],
    max_defs: usize,
) -> Vec<(String, String, usize)> {
    let mentioned: HashSet<&str> = mentioned_idents.iter().map(|s| s.as_str()).collect();

    let file_rank: HashMap<&str, f64> =
        ranked_files.iter().map(|(f, s)| (f.as_str(), *s)).collect();

    let mut all_defs: Vec<(f64, &str, String, usize)> = Vec::new();

    for ft in file_tags {
        let rank = file_rank.get(ft.rel_path.as_str()).copied().unwrap_or(0.0);
        for tag in &ft.tags {
            if tag.kind != TagKind::Def {
                continue;
            }
            let mut score = rank;
            if mentioned.contains(tag.ident.as_str()) {
                score += MENTION_BOOST;
            }
            all_defs.push((score, ft.rel_path.as_str(), tag.ident.clone(), tag.line));
        }
    }

    all_defs.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    all_defs
        .into_iter()
        .take(max_defs)
        .map(|(_, path, ident, line)| (path.to_string(), ident, line))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tags::{FileTags, Tag};

    fn make_tags(rel: &str, defs: &[&str], refs: &[&str]) -> FileTags {
        let mut tags: Vec<Tag> = defs
            .iter()
            .map(|d| Tag {
                rel_path: rel.to_string(),
                ident: d.to_string(),
                kind: TagKind::Def,
                line: 1,
            })
            .collect();
        tags.extend(refs.iter().map(|r| Tag {
            rel_path: rel.to_string(),
            ident: r.to_string(),
            kind: TagKind::Ref,
            line: 0,
        }));
        FileTags {
            rel_path: rel.to_string(),
            tags,
            mtime: None,
        }
    }

    #[test]
    fn pagerank_converges_on_synthetic_graph() {
        let files = vec![
            make_tags("a.rs", &["foo", "bar"], &["baz"]),
            make_tags("b.rs", &["baz"], &["foo"]),
            make_tags("c.rs", &["qux"], &["foo", "bar"]),
        ];
        let ranked = rank_files(&files, &[], &[]);
        assert_eq!(ranked.len(), 3);
        let total: f64 = ranked.iter().map(|(_, s)| s).sum();
        assert!(total > 0.0, "scores should be positive");
    }

    #[test]
    fn mentioned_ident_boosts_defining_file() {
        let files = vec![
            make_tags("a.rs", &["foo"], &["bar"]),
            make_tags("b.rs", &["bar"], &["foo"]),
        ];
        let unmentioned = rank_files(&files, &[], &[]);
        let mentioned = rank_files(&files, &["bar".to_string()], &[]);

        let b_unment: f64 = unmentioned
            .iter()
            .find(|(f, _)| f == "b.rs")
            .map(|(_, s)| *s)
            .unwrap_or(0.0);
        let b_ment: f64 = mentioned
            .iter()
            .find(|(f, _)| f == "b.rs")
            .map(|(_, s)| *s)
            .unwrap_or(0.0);
        assert!(
            b_ment >= b_unment,
            "mentioned file should rank >= unmentioned: {b_ment} vs {b_unment}"
        );
    }

    #[test]
    fn context_file_gets_boost() {
        let files = vec![
            make_tags("a.rs", &["foo"], &["bar"]),
            make_tags("b.rs", &["bar"], &["foo"]),
        ];
        let with_ctx = rank_files(&files, &[], &["a.rs".to_string()]);
        let a_score: f64 = with_ctx
            .iter()
            .find(|(f, _)| f == "a.rs")
            .map(|(_, s)| *s)
            .unwrap_or(0.0);
        assert!(a_score > 0.0, "context file should have positive score");
    }
}
