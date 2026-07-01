use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use superposition_types::database::models::experimentation::{
    Bucket, Buckets, Experiment, ExperimentGroup, ExperimentStatusType, ExperimentType,
    GroupType, Variant, Variants,
};
use superposition_types::experimental::{Experimental, ExperimentalVariants};
use superposition_types::{logic::evaluate_local_cohorts, Condition, DimensionInfo};

use std::fmt;

pub trait MapError<T> {
    fn map_err_to_string(self) -> Result<T, String>;
}

impl<T, E> MapError<T> for Result<T, E>
where
    E: fmt::Display,
{
    fn map_err_to_string(self) -> Result<T, String> {
        self.map_err(|e| e.to_string())
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, uniffi::Record)]
pub struct FfiExperiment {
    pub id: String,
    pub traffic_percentage: u8,
    pub variants: Variants,
    pub context: Condition,
    pub status: ExperimentStatusType,
    /// Defaulted for backward compatibility with payloads predating RELEASE.
    #[serde(default)]
    pub experiment_type: ExperimentType,
}

impl Experimental for FfiExperiment {
    fn get_condition(&self) -> &Condition {
        &self.context
    }
}

impl ExperimentalVariants for FfiExperiment {
    fn get_variants_mut(&mut self) -> &mut Vec<Variant> {
        &mut self.variants
    }
}

impl From<Experiment> for FfiExperiment {
    fn from(experiment: Experiment) -> Self {
        Self {
            id: experiment.id.to_string(),
            traffic_percentage: *experiment.traffic_percentage,
            variants: experiment.variants,
            context: experiment.context,
            status: experiment.status,
            experiment_type: experiment.experiment_type,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, uniffi::Record)]
pub struct FfiExperimentGroup {
    pub id: String,
    pub context: Condition,
    pub traffic_percentage: u8,
    pub member_experiment_ids: Vec<String>,
    pub group_type: GroupType,
    pub buckets: Buckets,
}

impl Experimental for FfiExperimentGroup {
    fn get_condition(&self) -> &Condition {
        &self.context
    }
}

impl From<ExperimentGroup> for FfiExperimentGroup {
    fn from(experiment_group: ExperimentGroup) -> Self {
        Self {
            id: experiment_group.id.to_string(),
            context: experiment_group.context,
            traffic_percentage: *experiment_group.traffic_percentage,
            member_experiment_ids: experiment_group
                .member_experiment_ids
                .iter()
                .map(|id| id.to_string())
                .collect(),
            group_type: experiment_group.group_type,
            buckets: experiment_group.buckets,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, uniffi::Record)]
pub struct ExperimentationArgs {
    pub experiments: Vec<FfiExperiment>,
    pub experiment_groups: Vec<FfiExperimentGroup>,
    // Named as per OpenFeature verbiage.
    pub targeting_key: String,
}

pub type Experiments = Vec<FfiExperiment>;

pub type ExperimentGroups = Vec<FfiExperimentGroup>;

#[derive(Debug, Clone, uniffi::Record)]
pub struct ExperimentConfig {
    pub experiments: Experiments,
    pub experiment_groups: ExperimentGroups,
}

pub fn get_applicable_variants(
    dimensions_info: &HashMap<String, DimensionInfo>,
    experiments: Experiments,
    experiment_groups: &ExperimentGroups,
    query_data: &Map<String, Value>,
    identifier: &str,
    prefix: Option<Vec<String>>,
) -> Vec<String> {
    let context = evaluate_local_cohorts(dimensions_info, query_data);

    let buckets =
        get_applicable_buckets_from_group(experiment_groups, &context, identifier);

    let experiments: HashMap<String, FfiExperiment> =
        get_satisfied_experiments(experiments, &context, prefix)
            .into_iter()
            .map(|exp| (exp.id.clone(), exp))
            .collect();

    get_applicable_variants_from_group_response(&experiments, &context, &buckets)
}

pub fn get_applicable_buckets_from_group(
    experiment_groups: &ExperimentGroups,
    context: &Map<String, Value>,
    identifier: &str,
) -> Vec<(usize, Bucket)> {
    if identifier.is_empty() {
        return vec![];
    }

    experiment_groups
        .iter()
        .filter_map(|exp_group| {
            let hashed_percentage = calculate_bucket_index(identifier, &exp_group.id);
            log::info!(
                "Identifier: {}, Experiment Group ID: {}, Hashed Percentage: {}",
                identifier,
                exp_group.id,
                hashed_percentage
            );
            let exp_context = &exp_group.context;

            let valid_context = superposition_types::apply(exp_context, context);

            let res =
                valid_context && exp_group.traffic_percentage >= hashed_percentage as u8;

            res.then_some(
                exp_group
                    .buckets
                    .get(hashed_percentage)
                    .and_then(Clone::clone),
            )
            .flatten()
            .and_then(|b| {
                if exp_group.group_type == GroupType::SystemGenerated {
                    Some((hashed_percentage, b))
                } else if exp_group.traffic_percentage > 0 {
                    Some((
                        (hashed_percentage * 100) / exp_group.traffic_percentage as usize,
                        b,
                    ))
                } else {
                    None
                }
            })
        })
        .collect()
}

pub fn get_applicable_variants_from_group_response(
    experiments: &HashMap<String, FfiExperiment>,
    context: &Map<String, Value>,
    bucket_response: &[(usize, Bucket)],
) -> Vec<String> {
    bucket_response
        .iter()
        .filter_map(|(toss, bucket)| {
            experiments.get(&bucket.experiment_id).and_then(|exp| {
                let valid_context = superposition_types::apply(&exp.context, context);

                let res = valid_context
                    && (exp.experiment_type.total_traffic_percentage(
                        exp.traffic_percentage,
                        exp.variants.len(),
                    ) as usize)
                        >= *toss;

                res.then_some(bucket.variant_id.clone())
            })
        })
        .collect()
}

#[inline]
pub fn calculate_bucket_index(identifier: &str, group_id: &str) -> usize {
    let mut hasher = DefaultHasher::new();
    (identifier, group_id).hash(&mut hasher);
    (hasher.finish() % 100) as usize
}

pub fn get_satisfied_experiments(
    mut experiments: Experiments,
    context: &Map<String, Value>,
    filter_prefixes: Option<Vec<String>>,
) -> Experiments {
    if let Some(prefix_list) = filter_prefixes.filter(|p| !p.is_empty()) {
        let prefix_list: HashSet<String> = HashSet::from_iter(prefix_list);
        experiments = FfiExperiment::filter_keys_by_prefix(experiments, &prefix_list);
    }

    if !context.is_empty() {
        experiments = FfiExperiment::get_satisfied(experiments, context);
    }

    experiments
}

pub fn filter_experiments_by_context(
    mut experiments: Experiments,
    context: &Map<String, Value>,
    filter_prefixes: Option<Vec<String>>,
) -> Experiments {
    if let Some(prefix_list) = filter_prefixes.filter(|p| !p.is_empty()) {
        let prefix_list: HashSet<String> = HashSet::from_iter(prefix_list);
        experiments = FfiExperiment::filter_keys_by_prefix(experiments, &prefix_list);
    }

    if !context.is_empty() {
        experiments = FfiExperiment::filter_by_eval(experiments, context);
    }

    experiments
}

#[cfg(test)]
mod tests {
    use super::*;
    use superposition_types::database::models::experimentation::VariantType;
    use superposition_types::{Exp, Overrides};

    fn variant(id: &str, variant_type: VariantType) -> Variant {
        let overrides =
            Map::from_iter([("key".to_string(), Value::String(id.to_string()))]);
        Variant {
            id: id.to_string(),
            variant_type,
            context_id: None,
            override_id: None,
            overrides: Exp::<Overrides>::try_from(overrides).unwrap(),
        }
    }

    fn experiment(experiment_type: ExperimentType, traffic: u8) -> FfiExperiment {
        FfiExperiment {
            id: "exp1".to_string(),
            traffic_percentage: traffic,
            variants: Variants::new(vec![
                variant("exp1-control", VariantType::CONTROL),
                variant("exp1-experimental-1", VariantType::EXPERIMENTAL),
            ]),
            context: Exp::<Condition>::try_from(Map::new()).unwrap().into_inner(),
            status: ExperimentStatusType::INPROGRESS,
            experiment_type,
        }
    }

    // Resolve the variant a single bucket at `toss` maps to through the gate.
    fn resolve(exp: &FfiExperiment, toss: usize, variant_id: &str) -> Vec<String> {
        let experiments =
            HashMap::from([(exp.id.clone(), exp.clone())]);
        let buckets = vec![(
            toss,
            Bucket {
                variant_id: variant_id.to_string(),
                experiment_id: exp.id.clone(),
            },
        )];
        get_applicable_variants_from_group_response(&experiments, &Map::new(), &buckets)
    }

    #[test]
    fn release_serves_buckets_above_old_cap() {
        // RELEASE @ 40%: control absorbs the remainder, so ALL 100 buckets serve.
        // A high toss (90) that the old `traffic * variants` (= 80) gate wrongly
        // dropped — leaking to base config — must now resolve to its variant.
        let exp = experiment(ExperimentType::Release, 40);
        assert_eq!(resolve(&exp, 90, "exp1-control"), vec!["exp1-control"]);
        assert_eq!(resolve(&exp, 99, "exp1-control"), vec!["exp1-control"]);
    }

    #[test]
    fn default_still_gates_above_coverage() {
        // DEFAULT @ 40% x 2 variants covers 80 buckets; toss 90 falls through,
        // toss 70 resolves. (Unchanged behaviour — guards against over-correction.)
        let exp = experiment(ExperimentType::Default, 40);
        assert!(resolve(&exp, 90, "exp1-experimental-1").is_empty());
        assert_eq!(
            resolve(&exp, 70, "exp1-experimental-1"),
            vec!["exp1-experimental-1"]
        );
    }
}
