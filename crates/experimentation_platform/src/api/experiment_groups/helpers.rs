use std::collections::{HashMap, HashSet};

use actix_web::web::{Data, Json};
use chrono::{DateTime, Utc};
use diesel::{
    BoolExpressionMethods, ExpressionMethods, QueryDsl, RunQueryDsl, SelectableHelper,
};
use fred::{prelude::RedisPool, types::Expiration};
use serde_json::Value;
use service_utils::{
    helpers::{generate_snowflake_id, get_from_env_or_default},
    redis::{
        EXPERIMENT_CONFIG_LAST_MODIFIED_KEY_SUFFIX,
        EXPERIMENT_GROUPS_LAST_MODIFIED_KEY_SUFFIX, EXPERIMENT_GROUPS_LIST_KEY_SUFFIX,
        redis_set_data,
    },
    service::types::{AppState, SchemaName, WorkspaceContext},
};
use superposition_macros::{bad_argument, unexpected_error};
use superposition_types::{
    Condition, DBConnection, PaginatedResponse, User,
    api::experiment_groups::ExpGroupMemberRequest,
    database::{
        models::{
            ChangeReason, Description,
            experimentation::{
                Bucket, Buckets, Experiment, ExperimentGroup, ExperimentStatusType,
                ExperimentType, GroupType, TrafficPercentage, VariantType,
            },
        },
        schema::{
            event_log::dsl as event_log, experiment_groups::dsl as experiment_groups,
            experiments::dsl as experiments,
        },
    },
    result as superposition,
};

use crate::api::experiments::helpers::{ensure_experiments_exist, hash};

pub fn fetch_and_validate_members(
    new_members: &[i64],
    existing_members: &[i64],
    conn: &mut DBConnection,
    schema_name: &SchemaName,
) -> superposition::Result<Vec<Experiment>> {
    if new_members.is_empty() {
        return Ok(Vec::new());
    }
    let new_members = HashSet::from_iter(new_members.to_owned());
    let existing_members = HashSet::from_iter(existing_members.to_owned());
    let repeating_members = new_members
        .intersection(&existing_members)
        .collect::<Vec<_>>();
    if !repeating_members.is_empty() {
        return Err(bad_argument!(
            "The new members list contains IDs that are already in the group: {}",
            repeating_members
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let members: Vec<Experiment> = experiments::experiments
        .filter(
            experiments::id
                .eq_any(&new_members)
                .and(experiments::status.eq(ExperimentStatusType::CREATED)),
        )
        .schema_name(schema_name)
        .get_results::<Experiment>(conn)?;

    ensure_experiments_exist(
        &new_members,
        &members,
        "The following experiment IDs are not present in the database/are not in the created stage",
    )?;
    Ok(members)
}

/// validates if the members in the members lit can be part of the experiment group
/// it checks the following
/// - if their contexts contain the group context
/// - if the sum of their traffic percentages does not exceed 100%
pub fn validate_experiment_group_constraints(
    member_experiments: &[Experiment],
    existing_members: &[i64],
    group_context: &Condition,
) -> superposition::Result<Vec<i64>> {
    let existing_members = HashSet::from_iter(existing_members.to_owned());

    for member_experiment in member_experiments.iter() {
        if !member_experiment
            .context
            .contains(group_context)
            .map_err(|e| bad_argument!("The contexts do not match. Error: {}", e))?
        {
            return Err(bad_argument!(
                "Experiment with id {} does not fit in with the experiment group. The contexts do not match.",
                member_experiment.id
            ));
        }
    }

    let all_members = member_experiments
        .iter()
        .map(|exp| exp.id)
        .collect::<HashSet<i64>>()
        .union(&existing_members)
        .cloned()
        .collect::<Vec<_>>();
    Ok(all_members)
}

pub fn add_members(
    exp_group_id: &i64,
    member_experiments: &[Experiment],
    mut req: ExpGroupMemberRequest,
    conn: &mut DBConnection,
    schema_name: &SchemaName,
    user: &User,
) -> superposition::Result<Json<ExperimentGroup>> {
    if req.member_experiment_ids.is_empty() {
        return Err(bad_argument!(
            "Please provide at least one experiment ID to add to the group"
        ));
    }
    let experiment_group = fetch_experiment_group(exp_group_id, conn, schema_name)?;

    if experiment_group.group_type == GroupType::SystemGenerated {
        return Err(bad_argument!(
            "Cannot add members to a system-generated experiment groups."
        ));
    }

    // RELEASE experiments allocate all 100 buckets (control absorbs the
    // remainder), so they cannot share a user-created group with other members.
    // They run in their own system-generated group created at ramp time.
    if member_experiments
        .iter()
        .any(|exp| exp.experiment_type == ExperimentType::Release)
    {
        return Err(bad_argument!(
            "RELEASE experiments cannot be added to a user-created experiment group; they run in their own system-generated group."
        ));
    }

    req.member_experiment_ids = validate_experiment_group_constraints(
        member_experiments,
        &experiment_group.member_experiment_ids,
        &experiment_group.context,
    )?;

    let updated_group = diesel::update(experiment_groups::experiment_groups)
        .filter(experiment_groups::id.eq(exp_group_id))
        .set((
            req,
            experiment_groups::last_modified_by.eq(user.get_email()),
            experiment_groups::last_modified_at.eq(chrono::Utc::now()),
        ))
        .returning(ExperimentGroup::as_returning())
        .schema_name(schema_name)
        .get_result(conn)?;
    Ok(Json(updated_group))
}

pub fn remove_members(
    id: &i64,
    mut req: ExpGroupMemberRequest,
    conn: &mut DBConnection,
    schema_name: &SchemaName,
    user: &User,
) -> superposition::Result<Json<ExperimentGroup>> {
    if req.member_experiment_ids.is_empty() {
        return Err(bad_argument!(
            "Please provide at least one experiment ID to remove from the group"
        ));
    }

    let experiment_group = fetch_experiment_group(id, conn, schema_name)?;

    if experiment_group.group_type == GroupType::SystemGenerated {
        return Err(bad_argument!(
            "Cannot remove members from a system-generated experiment group."
        ));
    }

    let current_members: HashSet<i64> =
        HashSet::from_iter(experiment_group.member_experiment_ids.clone());
    let members_to_remove = HashSet::from_iter(req.member_experiment_ids);
    req.member_experiment_ids = current_members
        .difference(&members_to_remove)
        .cloned()
        .collect::<Vec<_>>();

    let experiments_to_remove: Vec<Experiment> = experiments::experiments
        .filter(experiments::id.eq_any(members_to_remove.clone()))
        .schema_name(schema_name)
        .for_update()
        .get_results::<Experiment>(conn)?;

    ensure_experiments_exist(
        &HashSet::from_iter(members_to_remove),
        &experiments_to_remove,
        "The following experiment IDs are not present in the database",
    )?;

    let mut buckets = experiment_group.buckets;
    for member_experiment in &experiments_to_remove {
        free_experiment_buckets(member_experiment, &mut buckets);
    }

    let updated_group = diesel::update(experiment_groups::experiment_groups)
        .filter(experiment_groups::id.eq(&id))
        .set((
            req,
            experiment_groups::buckets.eq(buckets),
            experiment_groups::last_modified_by.eq(user.get_email()),
            experiment_groups::last_modified_at.eq(chrono::Utc::now()),
        ))
        .returning(ExperimentGroup::as_returning())
        .schema_name(schema_name)
        .get_result(conn)?;
    Ok(Json(updated_group))
}

/// Per-variant bucket targets (out of 100) for an experiment, derived from its
/// type. The sum of the targets is the number of buckets that will be assigned.
///
/// - `Release`: the control variant absorbs the remaining `100 - X`%, and the
///   `X`% is split equally across the experimental variants (the `X % E`
///   remainder going to the first experimental variants). The targets always
///   sum to 100 — every matching request lands on a variant, no fall-through.
/// - `Default` / `DeleteOverrides`: every variant gets `X` buckets; the
///   remaining `100 - X * N` buckets stay unassigned and fall through to the
///   base config (existing behaviour).
fn variant_bucket_targets(
    experiment: &Experiment,
    exp_traffic_percentage: &TrafficPercentage,
) -> Vec<(String, usize)> {
    let x = **exp_traffic_percentage as usize;
    match experiment.experiment_type {
        ExperimentType::Release => {
            let experimental_count = experiment
                .variants
                .iter()
                .filter(|v| v.variant_type == VariantType::EXPERIMENTAL)
                .count();
            let (base, remainder) = if experimental_count == 0 {
                (0, 0)
            } else {
                (x / experimental_count, x % experimental_count)
            };
            let mut experimental_idx = 0;
            experiment
                .variants
                .iter()
                .map(|variant| {
                    let target = match variant.variant_type {
                        VariantType::CONTROL => 100usize.saturating_sub(x),
                        VariantType::EXPERIMENTAL => {
                            let extra = usize::from(experimental_idx < remainder);
                            experimental_idx += 1;
                            base + extra
                        }
                    };
                    (variant.id.clone(), target)
                })
                .collect()
        }
        ExperimentType::Default | ExperimentType::DeleteOverrides => experiment
            .variants
            .iter()
            .map(|variant| (variant.id.clone(), x))
            .collect(),
    }
}

pub fn update_bucket_allocation(
    experiment: &Experiment,
    exp_group_buckets: &mut Buckets,
    exp_traffic_percentage: &TrafficPercentage,
) -> superposition::Result<()> {
    let targets = variant_bucket_targets(experiment, exp_traffic_percentage);
    reconcile_buckets(experiment, exp_group_buckets, &targets)
}

/// Reconcile the group's buckets so each of the experiment's variants holds
/// exactly its target number of buckets.
///
/// A single scalar diff cannot express RELEASE, where the control and
/// experimental variants move in opposite directions as `X` changes (ramping up
/// shrinks control and grows experimental). So we reconcile per variant and,
/// crucially, free over-target variants *before* filling under-target ones —
/// otherwise (RELEASE keeps all 100 slots filled) there would be no empty slot
/// to grow into.
fn reconcile_buckets(
    experiment: &Experiment,
    exp_group_buckets: &mut Buckets,
    targets: &[(String, usize)],
) -> superposition::Result<()> {
    let experiment_id = experiment.id.to_string();
    let target_ids: HashSet<&str> = targets.iter().map(|(id, _)| id.as_str()).collect();

    // Free buckets belonging to this experiment but to variants that no longer
    // exist on it (e.g. a variant was removed).
    for bucket in exp_group_buckets.iter_mut() {
        let stale = bucket.as_ref().is_some_and(|b| {
            b.experiment_id == experiment_id
                && !target_ids.contains(b.variant_id.as_str())
        });
        if stale {
            *bucket = None;
        }
    }

    // Current bucket count held by each of this experiment's variants. Owns its
    // keys so the immutable borrow of the buckets is released before we mutate.
    let mut current_counts: HashMap<String, usize> = HashMap::new();
    for b in exp_group_buckets.iter().flatten() {
        if b.experiment_id == experiment_id {
            *current_counts.entry(b.variant_id.clone()).or_insert(0) += 1;
        }
    }

    // Pass 1: free excess buckets from over-target variants back to the pool.
    for (variant_id, target) in targets {
        let current = current_counts
            .get(variant_id.as_str())
            .copied()
            .unwrap_or(0);
        let mut to_remove = current.saturating_sub(*target);
        if to_remove == 0 {
            continue;
        }
        for bucket in exp_group_buckets.iter_mut() {
            if to_remove == 0 {
                break;
            }
            let matches = bucket.as_ref().is_some_and(|b| {
                b.experiment_id == experiment_id && b.variant_id == *variant_id
            });
            if matches {
                *bucket = None;
                to_remove -= 1;
            }
        }
    }

    // Pass 2: fill under-target variants from the now-replenished empty pool.
    for (variant_id, target) in targets {
        let current = current_counts
            .get(variant_id.as_str())
            .copied()
            .unwrap_or(0);
        let mut to_add = target.saturating_sub(current);
        if to_add == 0 {
            continue;
        }
        for bucket in exp_group_buckets.iter_mut() {
            if to_add == 0 {
                break;
            }
            if bucket.is_none() {
                *bucket = Some(Bucket {
                    experiment_id: experiment_id.clone(),
                    variant_id: variant_id.clone(),
                });
                to_add -= 1;
            }
        }
        if to_add > 0 {
            return Err(bad_argument!(
                "Not enough empty buckets to accommodate the updated traffic percentage. Required additional: {} for variant {}",
                to_add,
                variant_id
            ));
        }
    }

    Ok(())
}

/// Free every bucket held by an experiment, unconditionally. Used when detaching
/// or removing an experiment from a group — unlike running the type-aware
/// allocator at `X = 0` (which for RELEASE would keep the control variant's 100
/// buckets), this always clears the experiment out entirely.
pub fn free_experiment_buckets(experiment: &Experiment, exp_group_buckets: &mut Buckets) {
    let experiment_id = experiment.id.to_string();
    for bucket in exp_group_buckets.iter_mut() {
        let owned = bucket
            .as_ref()
            .is_some_and(|b| b.experiment_id == experiment_id);
        if owned {
            *bucket = None;
        }
    }
}

pub fn detach_experiment_from_group(
    experiment: &Experiment,
    experiment_group_id: i64,
    conn: &mut DBConnection,
    workspace_context: &WorkspaceContext,
    user: &User,
) -> superposition::Result<()> {
    let experiment_group = fetch_experiment_group(
        &experiment_group_id,
        conn,
        &workspace_context.schema_name,
    )?;

    let mut buckets = experiment_group.buckets;
    free_experiment_buckets(experiment, &mut buckets);

    let mut member_experiment_ids = experiment_group.member_experiment_ids;
    member_experiment_ids.retain(|&id| id != experiment.id);

    diesel::update(experiment_groups::experiment_groups)
        .filter(experiment_groups::id.eq(&experiment_group_id))
        .set((
            experiment_groups::change_reason.eq(ChangeReason::try_from(format!(
                "Removed experiment {} from group {}",
                experiment.id, experiment_group_id
            ))
            .map_err(|e| unexpected_error!(e))?),
            experiment_groups::member_experiment_ids.eq(member_experiment_ids),
            experiment_groups::buckets.eq(buckets),
            experiment_groups::last_modified_by.eq(user.get_email()),
            experiment_groups::last_modified_at.eq(chrono::Utc::now()),
        ))
        .returning(ExperimentGroup::as_returning())
        .schema_name(&workspace_context.schema_name)
        .execute(conn)?;

    if experiment_group.group_type == GroupType::SystemGenerated {
        diesel::delete(experiment_groups::experiment_groups)
            .filter(experiment_groups::id.eq(&experiment_group_id))
            .schema_name(&workspace_context.schema_name)
            .execute(conn)?;
    }

    Ok(())
}

pub fn create_system_generated_experiment_group(
    experiment: &Experiment,
    exp_traffic_percentage: &TrafficPercentage,
    state: &Data<AppState>,
    conn: &mut DBConnection,
    schema_name: &SchemaName,
    user: &User,
) -> superposition::Result<ExperimentGroup> {
    let context = experiment.context.clone();
    let id = generate_snowflake_id(state)?;
    let context_hash = hash(&Value::Object(context.clone().into()));
    let now = chrono::Utc::now();

    let group_traffic_percentage = TrafficPercentage::try_from(
        experiment
            .experiment_type
            .total_traffic_percentage(**exp_traffic_percentage, experiment.variants.len())
            as i32,
    )
    .map_err(|e| unexpected_error!(e))?;

    let mut buckets = Buckets::default();
    update_bucket_allocation(experiment, &mut buckets, exp_traffic_percentage)?;

    let new_experiment_group = ExperimentGroup {
        id,
        context_hash,
        name: experiment.name.clone(),
        description: Description::try_from(format!(
            "Experiment group for experiment {}",
            experiment.name
        ))
        .map_err(|e| unexpected_error!(e))?,
        change_reason: ChangeReason::try_from(format!(
            "System generated experiment group for experiment {}",
            experiment.id
        ))
        .map_err(|e| unexpected_error!(e))?,
        created_by: user.get_email(),
        last_modified_by: user.get_email(),
        created_at: now,
        last_modified_at: now,
        context,
        traffic_percentage: group_traffic_percentage,
        member_experiment_ids: vec![experiment.id],
        buckets,
        group_type: GroupType::SystemGenerated,
    };
    let new_experiment_group = diesel::insert_into(experiment_groups::experiment_groups)
        .values(&new_experiment_group)
        .returning(ExperimentGroup::as_returning())
        .schema_name(schema_name)
        .get_result::<ExperimentGroup>(conn)?;
    Ok(new_experiment_group)
}

pub fn update_experiment_group_buckets(
    experiment: &Experiment,
    experiment_group_id: &i64,
    exp_traffic_percentage: &TrafficPercentage,
    conn: &mut DBConnection,
    schema_name: &SchemaName,
    user: &User,
) -> superposition::Result<()> {
    let experiment_group =
        fetch_experiment_group(experiment_group_id, conn, schema_name)?;

    let new_traffic_percentage = match experiment_group.group_type {
        GroupType::SystemGenerated => TrafficPercentage::try_from(
            experiment
                .experiment_type
                .total_traffic_percentage(**exp_traffic_percentage, experiment.variants.len())
                as i32,
        )
        .map_err(|e| unexpected_error!(e))?,
        GroupType::UserCreated => experiment_group.traffic_percentage,
    };

    let mut buckets = experiment_group.buckets;
    update_bucket_allocation(experiment, &mut buckets, exp_traffic_percentage)?;

    diesel::update(experiment_groups::experiment_groups)
        .filter(experiment_groups::id.eq(experiment_group.id))
        .set((
            experiment_groups::buckets.eq(buckets),
            experiment_groups::traffic_percentage.eq(new_traffic_percentage),
            experiment_groups::change_reason.eq(ChangeReason::try_from(format!(
                "Updated traffic percentage for experiment group {}",
                experiment_group.id
            ))
            .map_err(|e| unexpected_error!(e))?),
            experiment_groups::last_modified_by.eq(user.get_email()),
            experiment_groups::last_modified_at.eq(chrono::Utc::now()),
        ))
        .returning(ExperimentGroup::as_returning())
        .schema_name(schema_name)
        .execute(conn)?;
    Ok(())
}

pub fn fetch_experiment_group(
    id: &i64,
    conn: &mut DBConnection,
    schema_name: &SchemaName,
) -> superposition::Result<ExperimentGroup> {
    let experiment_group = experiment_groups::experiment_groups
        .filter(experiment_groups::id.eq(id))
        .schema_name(schema_name)
        .for_update()
        .get_result::<ExperimentGroup>(conn)?;
    Ok(experiment_group)
}

pub async fn put_experiment_groups_in_redis(
    redis_pool: &Option<RedisPool>,
    conn: &mut DBConnection,
    schema_name: &SchemaName,
) -> superposition::Result<()> {
    let pool = match redis_pool {
        Some(pool) => pool,
        None => {
            log::debug!("Redis not configured, skipping experiment groups cache update");
            return Ok(());
        }
    };

    let experiment_group_list: Vec<ExperimentGroup> =
        experiment_groups::experiment_groups
            .order(experiment_groups::last_modified_at.desc())
            .schema_name(schema_name)
            .load::<ExperimentGroup>(conn)?;

    let paginated_response = PaginatedResponse::all(experiment_group_list);

    let serialized = serde_json::to_string(&paginated_response).map_err(|e| {
        log::error!("Failed to serialize experiment groups for redis: {}", e);
        unexpected_error!("Failed to serialize experiment groups for redis: {}", e)
    })?;

    let last_modified: Option<DateTime<Utc>> = event_log::event_log
        .filter(event_log::table_name.eq("experiment_groups"))
        .select(diesel::dsl::max(event_log::timestamp))
        .schema_name(schema_name)
        .first(conn)?;

    let key = format!("{}{EXPERIMENT_GROUPS_LIST_KEY_SUFFIX}", **schema_name);
    let last_modified_at_key = format!(
        "{}{EXPERIMENT_GROUPS_LAST_MODIFIED_KEY_SUFFIX}",
        **schema_name,
    );
    let config_modified_at_key = format!(
        "{}{EXPERIMENT_CONFIG_LAST_MODIFIED_KEY_SUFFIX}",
        **schema_name,
    );
    let last_modified = last_modified.map(|dt| dt.to_rfc2822()).unwrap_or_default();
    let key_ttl: i64 = get_from_env_or_default("REDIS_KEY_TTL", 604800);
    let expiration = Some(Expiration::EX(key_ttl));

    redis_set_data(
        pool,
        config_modified_at_key,
        last_modified.clone(),
        expiration.clone(),
    )
    .await?;

    redis_set_data(
        pool,
        last_modified_at_key,
        last_modified,
        expiration.clone(),
    )
    .await?;

    redis_set_data(pool, key, serialized, expiration).await?;

    log::debug!("Successfully updated experiment groups cache in Redis");
    Ok(())
}
