use chrono::Utc;
use experimentation_platform::api::experiment_groups::helpers as group_helpers;
use experimentation_platform::api::experiments::helpers;
use serde_json::{Map, Value, json};
use service_utils::service::types::ExperimentationFlags;
use superposition_types::{
    Condition, Exp, Overrides,
    database::models::{
        ChangeReason, Description, Metrics,
        experimentation::{
            Buckets, Experiment, ExperimentStatusType, ExperimentType, TrafficPercentage,
            Variant, VariantType, Variants,
        },
    },
    result as superposition,
};

enum Dimensions {
    Os(String),
    Client(String),
    #[allow(dead_code)]
    VariantIds(String),
}

fn multiple_dimension_ctx_gen(values: Vec<Dimensions>) -> Map<String, Value> {
    values
        .into_iter()
        .map(|val| {
            let (key, value) = match val {
                Dimensions::Os(os) => ("os".to_string(), json!(os)),
                Dimensions::Client(client_id) => {
                    ("clientId".to_string(), json!(client_id))
                }
                Dimensions::VariantIds(id) => ("variantIds".to_string(), json!(id)),
            };
            (key, value)
        })
        .collect::<Map<String, Value>>()
}

fn experiment_gen(
    override_keys: &[String],
    context: &Condition,
    status: ExperimentStatusType,
    variants: &[Variant],
) -> Experiment {
    Experiment {
        id: 123456789,
        created_at: Utc::now(),
        created_by: "test".to_string(),
        last_modified: Utc::now(),
        last_modified_by: "test".to_string(),
        name: "experiment-test".to_string(),
        experiment_type: ExperimentType::Default,
        traffic_percentage: TrafficPercentage::default(),
        started_at: None,
        started_by: None,

        override_keys: override_keys.to_vec(),
        status,
        context: context.clone(),
        variants: Variants::new(variants.to_owned()),
        chosen_variant: None,
        description: Description::try_from(String::from("test")).unwrap(),
        change_reason: ChangeReason::try_from(String::from("test")).unwrap(),
        metrics: Metrics::default(),
        experiment_group_id: None,
        idempotency_key: None,
    }
}

/************************* RELEASE experiment type *****************************************/

fn variant_gen(id: &str, variant_type: VariantType) -> Variant {
    Variant {
        id: id.to_string(),
        variant_type,
        context_id: None,
        override_id: None,
        overrides: Exp::<Overrides>::try_from(Map::from_iter(vec![(
            "key1".to_string(),
            json!("value1"),
        )]))
        .unwrap(),
    }
}

fn release_experiment_gen(variants: &[Variant]) -> Experiment {
    let context =
        Exp::<Condition>::try_from(multiple_dimension_ctx_gen(vec![Dimensions::Os(
            "os1".to_string(),
        )]))
        .unwrap()
        .into_inner();
    let mut experiment = experiment_gen(
        &["key1".to_string()],
        &context,
        ExperimentStatusType::CREATED,
        variants,
    );
    experiment.experiment_type = ExperimentType::Release;
    experiment
}

fn count_for(buckets: &Buckets, variant_id: &str) -> usize {
    buckets
        .iter()
        .filter(|b| b.as_ref().is_some_and(|x| x.variant_id == variant_id))
        .count()
}

fn total_assigned(buckets: &Buckets) -> usize {
    buckets.iter().filter(|b| b.is_some()).count()
}

#[test]
fn test_bucket_coverage_release_vs_default() {
    // RELEASE always covers all 100 buckets (control absorbs the remainder).
    assert_eq!(ExperimentType::Release.total_traffic_percentage(10, 2), 100);
    assert_eq!(ExperimentType::Release.total_traffic_percentage(0, 2), 100);
    // DEFAULT covers traffic_percentage * variants.
    assert_eq!(ExperimentType::Default.total_traffic_percentage(30, 2), 60);
    assert_eq!(ExperimentType::DeleteOverrides.total_traffic_percentage(25, 3), 75);
}

#[test]
fn test_release_bucket_allocation_split() -> superposition::Result<()> {
    let variants = vec![
        variant_gen("control", VariantType::CONTROL),
        variant_gen("experimental", VariantType::EXPERIMENTAL),
    ];
    let experiment = release_experiment_gen(&variants);
    let mut buckets = Buckets::default();

    group_helpers::update_bucket_allocation(
        &experiment,
        &mut buckets,
        &TrafficPercentage::try_from(10_i32).unwrap(),
    )?;

    // experimental gets X% (10), control absorbs the rest (90), total is 100.
    assert_eq!(count_for(&buckets, "experimental"), 10);
    assert_eq!(count_for(&buckets, "control"), 90);
    assert_eq!(total_assigned(&buckets), 100);
    Ok(())
}

#[test]
fn test_release_bucket_reallocation_moves_buckets() -> superposition::Result<()> {
    let variants = vec![
        variant_gen("control", VariantType::CONTROL),
        variant_gen("experimental", VariantType::EXPERIMENTAL),
    ];
    let experiment = release_experiment_gen(&variants);
    let mut buckets = Buckets::default();

    // Initial ramp to 10%.
    group_helpers::update_bucket_allocation(
        &experiment,
        &mut buckets,
        &TrafficPercentage::try_from(10_i32).unwrap(),
    )?;
    // Re-ramp to 50% — buckets must move from control to experimental (the
    // reconciler frees over-target variants before filling under-target ones).
    group_helpers::update_bucket_allocation(
        &experiment,
        &mut buckets,
        &TrafficPercentage::try_from(50_i32).unwrap(),
    )?;

    assert_eq!(count_for(&buckets, "experimental"), 50);
    assert_eq!(count_for(&buckets, "control"), 50);
    assert_eq!(total_assigned(&buckets), 100);

    // Full release — everyone gets the experimental variant.
    group_helpers::update_bucket_allocation(
        &experiment,
        &mut buckets,
        &TrafficPercentage::try_from(100_i32).unwrap(),
    )?;
    assert_eq!(count_for(&buckets, "experimental"), 100);
    assert_eq!(count_for(&buckets, "control"), 0);
    assert_eq!(total_assigned(&buckets), 100);
    Ok(())
}

#[test]
fn test_release_multi_experimental_split_with_remainder() -> superposition::Result<()> {
    // 1 control + 3 experimental, X = 10 → base 3 each, remainder 1 to the first.
    let variants = vec![
        variant_gen("control", VariantType::CONTROL),
        variant_gen("exp_a", VariantType::EXPERIMENTAL),
        variant_gen("exp_b", VariantType::EXPERIMENTAL),
        variant_gen("exp_c", VariantType::EXPERIMENTAL),
    ];
    let experiment = release_experiment_gen(&variants);
    let mut buckets = Buckets::default();

    group_helpers::update_bucket_allocation(
        &experiment,
        &mut buckets,
        &TrafficPercentage::try_from(10_i32).unwrap(),
    )?;

    let experimental_total = count_for(&buckets, "exp_a")
        + count_for(&buckets, "exp_b")
        + count_for(&buckets, "exp_c");
    assert_eq!(experimental_total, 10);
    assert_eq!(count_for(&buckets, "exp_a"), 4); // remainder lands on the first
    assert_eq!(count_for(&buckets, "exp_b"), 3);
    assert_eq!(count_for(&buckets, "exp_c"), 3);
    assert_eq!(count_for(&buckets, "control"), 90);
    assert_eq!(total_assigned(&buckets), 100);
    Ok(())
}

#[test]
fn test_release_free_experiment_buckets() -> superposition::Result<()> {
    let variants = vec![
        variant_gen("control", VariantType::CONTROL),
        variant_gen("experimental", VariantType::EXPERIMENTAL),
    ];
    let experiment = release_experiment_gen(&variants);
    let mut buckets = Buckets::default();
    group_helpers::update_bucket_allocation(
        &experiment,
        &mut buckets,
        &TrafficPercentage::try_from(40_i32).unwrap(),
    )?;
    assert_eq!(total_assigned(&buckets), 100);

    // Detaching/removing frees everything, even though control held buckets.
    group_helpers::free_experiment_buckets(&experiment, &mut buckets);
    assert_eq!(total_assigned(&buckets), 0);
    Ok(())
}

#[test]
fn test_duplicate_override_key_entries() {
    let override_keys = vec!["key1".to_string(), "key2".to_string(), "key1".to_string()];
    assert!(matches!(
        helpers::validate_override_keys(&override_keys),
        Err(superposition::AppError::BadArgument(_))
    ));
}

#[test]
fn test_unique_override_key_entries() {
    let override_keys = vec!["key1".to_string(), "key2".to_string()];
    assert!(matches!(
        helpers::validate_override_keys(&override_keys),
        Ok(())
    ));
}

#[test]
fn test_are_overlapping_contexts() -> Result<(), superposition::AppError> {
    let context_a = multiple_dimension_ctx_gen(vec![
        Dimensions::Os("os1".to_string()),
        Dimensions::Client("testclient1".to_string()),
    ]);
    let context_a = Exp::<Condition>::try_from(context_a.clone())
        .map_err(superposition::AppError::BadArgument)?
        .into_inner();

    let context_b = multiple_dimension_ctx_gen(vec![
        Dimensions::Os("os1".to_string()),
        Dimensions::Client("testclient2".to_string()),
    ]);
    let context_b = Exp::<Condition>::try_from(context_b.clone())
        .map_err(superposition::AppError::BadArgument)?
        .into_inner();

    let context_c = multiple_dimension_ctx_gen(vec![Dimensions::Os("os1".to_string())]);
    let context_d = multiple_dimension_ctx_gen(vec![Dimensions::Os("os2".to_string())]);
    let context_c = Exp::<Condition>::try_from(context_c.clone())
        .map_err(superposition::AppError::BadArgument)?
        .into_inner();
    let context_d = Exp::<Condition>::try_from(context_d.clone())
        .map_err(superposition::AppError::BadArgument)?
        .into_inner();

    // both contexts with same dimensions
    assert!(helpers::are_overlapping_contexts(&context_a, &context_a)?);
    // contexts with one different dimension
    assert!(!(helpers::are_overlapping_contexts(&context_a, &context_b)?));
    // one context dimensions are subset of other
    assert!(helpers::are_overlapping_contexts(&context_a, &context_c)?);
    // one context dimensions not a subset of other but have less dimensions that other
    assert!(!(helpers::are_overlapping_contexts(&context_a, &context_d)?));
    // disjoint contexts
    assert!(!(helpers::are_overlapping_contexts(&context_c, &context_d)?));
    Ok(())
}

#[test]
fn test_check_variants_override_coverage() -> Result<(), superposition::AppError> {
    let override_keys = vec!["key1".to_string(), "key2".to_string()];
    let overrides = [
        Exp::<Overrides>::try_from(Map::from_iter(vec![
            ("key1".to_string(), json!("value1")),
            ("key2".to_string(), json!("value2")),
        ])),
        // has one override key missing
        Exp::<Overrides>::try_from(Map::from_iter(vec![(
            "key1".to_string(),
            json!("value1"),
        )])),
        // has an unknown override key
        Exp::<Overrides>::try_from(Map::from_iter(vec![(
            "key3".to_string(),
            json!("value3"),
        )])),
        // has an extra unknown override key
        Exp::<Overrides>::try_from(Map::from_iter(vec![
            ("key1".to_string(), json!("value1")),
            ("key2".to_string(), json!("value2")),
            ("key3".to_string(), json!("value3")),
        ])),
    ]
    .into_iter()
    .map(|a| a.map(|b| b.into_inner()))
    .collect::<Result<Vec<Overrides>, String>>()
    .map_err(superposition::AppError::BadArgument)?;

    assert!(helpers::check_variant_override_coverage(
        &overrides[0],
        &override_keys
    ));
    assert!(!helpers::check_variant_override_coverage(
        &overrides[1],
        &override_keys
    ));
    assert!(!helpers::check_variant_override_coverage(
        &overrides[2],
        &override_keys
    ));
    assert!(!helpers::check_variant_override_coverage(
        &overrides[3],
        &override_keys
    ));
    Ok(())
}

/************************* No Restrictions *****************************************/

#[test]
fn test_is_valid_experiment_no_restrictions_overlapping_experiment()
-> Result<(), superposition::AppError> {
    let experiment_context = multiple_dimension_ctx_gen(vec![
        Dimensions::Os("os1".to_string()),
        Dimensions::Client("testclient1".to_string()),
    ]);
    let experiment_context = Exp::<Condition>::try_from(experiment_context.clone())
        .map_err(superposition::AppError::BadArgument)?
        .into_inner();
    let experiment_override_keys = vec!["key1".to_string(), "key2".to_string()];
    let flags = ExperimentationFlags {
        allow_same_keys_overlapping_ctx: true,
        allow_diff_keys_overlapping_ctx: true,
        allow_same_keys_non_overlapping_ctx: true,
    };

    let active_experiments = vec![experiment_gen(
        &["key1".to_string(), "key2".to_string()],
        &experiment_context,
        ExperimentStatusType::CREATED,
        &[],
    )];

    assert_eq!(
        helpers::is_valid_experiment(
            &experiment_context,
            &experiment_override_keys,
            &flags,
            &active_experiments
        )?,
        (true, "".to_string())
    );

    Ok(())
}

#[test]
fn test_is_valid_experiment_no_restrictions_non_overlapping_experiment()
-> Result<(), superposition::AppError> {
    let experiment_context = multiple_dimension_ctx_gen(vec![
        Dimensions::Os("os1".to_string()),
        Dimensions::Client("testclient1".to_string()),
    ]);
    let experiment_context = Exp::<Condition>::try_from(experiment_context.clone())
        .map_err(superposition::AppError::BadArgument)?
        .into_inner();
    let experiment_override_keys = vec!["key1".to_string(), "key2".to_string()];
    let flags = ExperimentationFlags {
        allow_same_keys_overlapping_ctx: true,
        allow_diff_keys_overlapping_ctx: true,
        allow_same_keys_non_overlapping_ctx: true,
    };

    let active_experiments = vec![experiment_gen(
        &["key1".to_string(), "key2".to_string()],
        &Exp::<Condition>::try_from(multiple_dimension_ctx_gen(vec![
            Dimensions::Os("os2".to_string()),
            Dimensions::Client("testclient2".to_string()),
        ]))
        .map_err(superposition::AppError::BadArgument)?
        .into_inner(),
        ExperimentStatusType::CREATED,
        &[],
    )];

    assert_eq!(
        helpers::is_valid_experiment(
            &experiment_context,
            &experiment_override_keys,
            &flags,
            &active_experiments
        )?,
        (true, "".to_string())
    );

    Ok(())
}

/************************* Restrict Same Keys Overlapping Context *****************************************/

#[test]
fn test_is_valid_experiment_restrict_same_keys_overlapping_ctx_overlapping_experiment_same_keys()
-> Result<(), superposition::AppError> {
    let experiment_context = multiple_dimension_ctx_gen(vec![
        Dimensions::Os("os1".to_string()),
        Dimensions::Client("testclient1".to_string()),
    ]);
    let experiment_context = Exp::<Condition>::try_from(experiment_context.clone())
        .map_err(superposition::AppError::BadArgument)?
        .into_inner();
    let experiment_override_keys = vec!["key1".to_string(), "key2".to_string()];
    let flags = ExperimentationFlags {
        allow_same_keys_overlapping_ctx: false,
        allow_diff_keys_overlapping_ctx: true,
        allow_same_keys_non_overlapping_ctx: true,
    };

    let active_experiments = vec![experiment_gen(
        &experiment_override_keys,
        &experiment_context,
        ExperimentStatusType::CREATED,
        &[],
    )];

    assert_eq!(
        helpers::is_valid_experiment(
            &experiment_context,
            &experiment_override_keys,
            &flags,
            &active_experiments
        )?,
        (false, "This current context overlaps with an existing experiment or the keys in the context are overlapping".to_string())
    );

    Ok(())
}

#[test]
fn test_is_valid_experiment_restrict_same_keys_overlapping_ctx_overlapping_experiment_one_same_key()
-> Result<(), superposition::AppError> {
    let experiment_context = multiple_dimension_ctx_gen(vec![
        Dimensions::Os("os1".to_string()),
        Dimensions::Client("testclient1".to_string()),
    ]);
    let experiment_context = Exp::<Condition>::try_from(experiment_context.clone())
        .map_err(superposition::AppError::BadArgument)?
        .into_inner();
    let experiment_override_keys = vec!["key1".to_string(), "key2".to_string()];
    let flags = ExperimentationFlags {
        allow_same_keys_overlapping_ctx: false,
        allow_diff_keys_overlapping_ctx: true,
        allow_same_keys_non_overlapping_ctx: true,
    };

    let active_experiments = vec![experiment_gen(
        &["key1".to_string(), "key3".to_string()],
        &experiment_context,
        ExperimentStatusType::CREATED,
        &[],
    )];

    assert_eq!(
        helpers::is_valid_experiment(
            &experiment_context,
            &experiment_override_keys,
            &flags,
            &active_experiments
        )?,
        (false, "This current context overlaps with an existing experiment or the keys in the context are overlapping".to_string())
    );

    Ok(())
}

#[test]
fn test_is_valid_experiment_restrict_same_keys_overlapping_ctx_overlapping_experiment_diff_keys()
-> Result<(), superposition::AppError> {
    let experiment_context = multiple_dimension_ctx_gen(vec![
        Dimensions::Os("os1".to_string()),
        Dimensions::Client("testclient1".to_string()),
    ]);
    let experiment_context = Exp::<Condition>::try_from(experiment_context.clone())
        .map_err(superposition::AppError::BadArgument)?
        .into_inner();
    let experiment_override_keys = vec!["key1".to_string(), "key2".to_string()];
    let flags = ExperimentationFlags {
        allow_same_keys_overlapping_ctx: false,
        allow_diff_keys_overlapping_ctx: true,
        allow_same_keys_non_overlapping_ctx: true,
    };

    let active_experiments = vec![experiment_gen(
        &["key3".to_string(), "key4".to_string()],
        &experiment_context,
        ExperimentStatusType::CREATED,
        &[],
    )];

    assert_eq!(
        helpers::is_valid_experiment(
            &experiment_context,
            &experiment_override_keys,
            &flags,
            &active_experiments
        )?,
        (true, "".to_string())
    );

    Ok(())
}

/************************* Restrict Different Keys Overlapping Context *****************************************/

#[test]
fn test_is_valid_experiment_restrict_diff_keys_overlapping_ctx_overlapping_experiment_same_keys()
-> Result<(), superposition::AppError> {
    let experiment_context = multiple_dimension_ctx_gen(vec![
        Dimensions::Os("os1".to_string()),
        Dimensions::Client("testclient1".to_string()),
    ]);
    let experiment_context = Exp::<Condition>::try_from(experiment_context.clone())
        .map_err(superposition::AppError::BadArgument)?
        .into_inner();
    let experiment_override_keys = vec!["key1".to_string(), "key2".to_string()];
    let flags = ExperimentationFlags {
        allow_same_keys_overlapping_ctx: true,
        allow_diff_keys_overlapping_ctx: false,
        allow_same_keys_non_overlapping_ctx: true,
    };

    let active_experiments = vec![experiment_gen(
        &experiment_override_keys,
        &experiment_context,
        ExperimentStatusType::CREATED,
        &[],
    )];

    assert_eq!(
        helpers::is_valid_experiment(
            &experiment_context,
            &experiment_override_keys,
            &flags,
            &active_experiments
        )?,
        (true, "".to_string())
    );

    Ok(())
}

#[test]
fn test_is_valid_experiment_restrict_diff_keys_overlapping_ctx_overlapping_experiment_one_diff_key()
-> Result<(), superposition::AppError> {
    let experiment_context = multiple_dimension_ctx_gen(vec![
        Dimensions::Os("os1".to_string()),
        Dimensions::Client("testclient1".to_string()),
    ]);
    let experiment_context = Exp::<Condition>::try_from(experiment_context.clone())
        .map_err(superposition::AppError::BadArgument)?
        .into_inner();
    let experiment_override_keys = vec!["key1".to_string(), "key2".to_string()];
    let flags = ExperimentationFlags {
        allow_same_keys_overlapping_ctx: true,
        allow_diff_keys_overlapping_ctx: false,
        allow_same_keys_non_overlapping_ctx: true,
    };

    let active_experiments = vec![experiment_gen(
        &["key1".to_string(), "key3".to_string()],
        &experiment_context,
        ExperimentStatusType::CREATED,
        &[],
    )];

    assert_eq!(
        helpers::is_valid_experiment(
            &experiment_context,
            &experiment_override_keys,
            &flags,
            &active_experiments
        )?,
        (false, "This current context overlaps with an existing experiment or the keys in the context are overlapping".to_string())
    );

    Ok(())
}

#[test]
fn test_is_valid_experiment_restrict_diff_keys_overlapping_ctx_overlapping_experiment_diff_keys()
-> Result<(), superposition::AppError> {
    let experiment_context = multiple_dimension_ctx_gen(vec![
        Dimensions::Os("os1".to_string()),
        Dimensions::Client("testclient1".to_string()),
    ]);
    let experiment_context = Exp::<Condition>::try_from(experiment_context.clone())
        .map_err(superposition::AppError::BadArgument)?
        .into_inner();
    let experiment_override_keys = vec!["key1".to_string(), "key2".to_string()];
    let flags = ExperimentationFlags {
        allow_same_keys_overlapping_ctx: true,
        allow_diff_keys_overlapping_ctx: false,
        allow_same_keys_non_overlapping_ctx: true,
    };

    let active_experiments = vec![experiment_gen(
        &["key3".to_string(), "key4".to_string()],
        &experiment_context,
        ExperimentStatusType::CREATED,
        &[],
    )];

    assert_eq!(
        helpers::is_valid_experiment(
            &experiment_context,
            &experiment_override_keys,
            &flags,
            &active_experiments
        )?,
        (false, "This current context overlaps with an existing experiment or the keys in the context are overlapping".to_string())
    );

    Ok(())
}

/************************* Restrict Same Keys Non Overlapping Context *****************************************/

#[test]
fn test_is_valid_experiment_restrict_same_keys_non_overlapping_ctx_non_overlapping_experiment_same_keys()
-> Result<(), superposition::AppError> {
    let experiment_context = multiple_dimension_ctx_gen(vec![
        Dimensions::Os("os1".to_string()),
        Dimensions::Client("testclient1".to_string()),
    ]);
    let experiment_context = Exp::<Condition>::try_from(experiment_context.clone())
        .map_err(superposition::AppError::BadArgument)?
        .into_inner();
    let experiment_override_keys = vec!["key1".to_string(), "key2".to_string()];
    let flags = ExperimentationFlags {
        allow_same_keys_overlapping_ctx: true,
        allow_diff_keys_overlapping_ctx: true,
        allow_same_keys_non_overlapping_ctx: false,
    };

    let active_experiments = vec![experiment_gen(
        &experiment_override_keys,
        &Exp::<Condition>::try_from(multiple_dimension_ctx_gen(vec![
            Dimensions::Os("os2".to_string()),
            Dimensions::Client("testclient2".to_string()),
        ]))
        .map_err(superposition::AppError::BadArgument)?
        .into_inner(),
        ExperimentStatusType::CREATED,
        &[],
    )];

    assert_eq!(
        helpers::is_valid_experiment(
            &experiment_context,
            &experiment_override_keys,
            &flags,
            &active_experiments
        )?,
        (false, "This current context overlaps with an existing experiment or the keys in the context are overlapping".to_string())
    );

    Ok(())
}

#[test]
fn test_is_valid_experiment_restrict_same_keys_non_overlapping_ctx_non_overlapping_experiment_one_diff_key()
-> Result<(), superposition::AppError> {
    let experiment_context = multiple_dimension_ctx_gen(vec![
        Dimensions::Os("os1".to_string()),
        Dimensions::Client("testclient1".to_string()),
    ]);
    let experiment_context = Exp::<Condition>::try_from(experiment_context.clone())
        .map_err(superposition::AppError::BadArgument)?
        .into_inner();
    let experiment_override_keys = vec!["key1".to_string(), "key2".to_string()];
    let flags = ExperimentationFlags {
        allow_same_keys_overlapping_ctx: true,
        allow_diff_keys_overlapping_ctx: true,
        allow_same_keys_non_overlapping_ctx: false,
    };

    let active_experiments = vec![experiment_gen(
        &["key1".to_string(), "key3".to_string()],
        &Exp::<Condition>::try_from(multiple_dimension_ctx_gen(vec![
            Dimensions::Os("os2".to_string()),
            Dimensions::Client("testclient2".to_string()),
        ]))
        .map_err(superposition::AppError::BadArgument)?
        .into_inner(),
        ExperimentStatusType::CREATED,
        &[],
    )];

    assert_eq!(
        helpers::is_valid_experiment(
            &experiment_context,
            &experiment_override_keys,
            &flags,
            &active_experiments
        )?,
        (false, "This current context overlaps with an existing experiment or the keys in the context are overlapping".to_string())
    );

    Ok(())
}

#[test]
fn test_is_valid_experiment_restrict_same_keys_non_overlapping_ctx_non_overlapping_experiment_diff_keys()
-> Result<(), superposition::AppError> {
    let experiment_context = multiple_dimension_ctx_gen(vec![
        Dimensions::Os("os1".to_string()),
        Dimensions::Client("testclient1".to_string()),
    ]);
    let experiment_context = Exp::<Condition>::try_from(experiment_context.clone())
        .map_err(superposition::AppError::BadArgument)?
        .into_inner();
    let experiment_override_keys = vec!["key1".to_string(), "key2".to_string()];
    let flags = ExperimentationFlags {
        allow_same_keys_overlapping_ctx: true,
        allow_diff_keys_overlapping_ctx: false,
        allow_same_keys_non_overlapping_ctx: true,
    };

    let active_experiments = vec![experiment_gen(
        &["key3".to_string(), "key4".to_string()],
        &Exp::<Condition>::try_from(multiple_dimension_ctx_gen(vec![
            Dimensions::Os("os2".to_string()),
            Dimensions::Client("testclient2".to_string()),
        ]))
        .map_err(superposition::AppError::BadArgument)?
        .into_inner(),
        ExperimentStatusType::CREATED,
        &[],
    )];

    assert_eq!(
        helpers::is_valid_experiment(
            &experiment_context,
            &experiment_override_keys,
            &flags,
            &active_experiments
        )?,
        (true, "".to_string())
    );

    Ok(())
}
