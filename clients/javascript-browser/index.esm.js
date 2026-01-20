// src/utils/deepMerge.ts
var isObject = (item) => {
  return item === Object(item) && !Array.isArray(item);
};
var deepMerge = (target, ...sources) => {
  if (!sources.length) {
    return target;
  }
  const result = target;
  if (isObject(result)) {
    const len = sources.length;
    for (let i = 0; i < len; i += 1) {
      const elm = sources[i];
      if (isObject(elm)) {
        for (const key in elm) {
          if (elm.hasOwnProperty(key)) {
            if (isObject(elm[key])) {
              if (!result[key] || !isObject(result[key])) {
                result[key] = {};
              }
              deepMerge(result[key], elm[key]);
            } else {
              if (Array.isArray(result[key]) && Array.isArray(elm[key])) {
                result[key] = [...elm[key]];
              } else {
                result[key] = elm[key];
              }
            }
          }
        }
      }
    }
  }
  return result;
};

// src/logic.ts
function applyLogic(condition, context, partial) {
  for (const dimension in condition) {
    if (condition.hasOwnProperty(dimension)) {
      const value = condition[dimension];
      if (dimension in context) {
        const contextValue = context[dimension];
        if (dimension === "variantIds") {
          if (Array.isArray(contextValue)) {
            if (contextValue.indexOf(value) === -1) {
              return false;
            }
          } else {
            return false;
          }
        } else if (!deepEqual(contextValue, value)) {
          return false;
        }
      } else if (partial) {
        continue;
      } else {
        return false;
      }
    }
  }
  return true;
}
function deepEqual(a, b) {
  if (typeof a !== typeof b) {
    return false;
  }
  if (typeof a === "object") {
    if (a === null || b === null) {
      return a === b;
    }
    if (Array.isArray(a) && Array.isArray(b)) {
      if (a.length !== b.length) {
        return false;
      }
      for (let i = 0; i < a.length; i++) {
        if (!deepEqual(a[i], b[i])) {
          return false;
        }
      }
      return true;
    }
    const keysA = Object.keys(a);
    const keysB = Object.keys(b);
    if (keysA.length !== keysB.length) {
      return false;
    }
    for (const key of keysA) {
      if (!deepEqual(a[key], b[key])) {
        return false;
      }
    }
    return true;
  }
  return a === b;
}
function apply(condition, context) {
  return applyLogic(condition, context, false);
}
function partialApply(condition, context) {
  return applyLogic(condition, context, true);
}
var logic_default = {
  apply,
  partialApply
};

// src/index.ts
var CacReader = class {
  constructor(completeConfig) {
    this.contexts = completeConfig.contexts;
    this.overrides = completeConfig.overrides;
    this.defaultConfig = completeConfig.default_configs;
  }
  evaluateConfig(data) {
    const requiredOverrides = [];
    for (let i = 0; i < this.contexts.length; i++) {
      try {
        if (logic_default.apply(this.contexts[i].condition, data)) {
          requiredOverrides.push(
            ...this.contexts[i].override_with_keys.map(
              (x) => this.overrides[x]
            )
          );
        }
      } catch (e) {
        console.error(e);
      }
    }
    const targetConfig = { ...this.defaultConfig };
    return deepMerge(targetConfig, ...requiredOverrides);
  }
};
var ExperimentReader = class {
  constructor(experiments) {
    this.experiments = experiments;
  }
  getApplicableVariants(data, toss) {
    if (!Number.isInteger(toss)) {
      throw new Error("Invalid toss, valid range: -1 to 100");
    }
    const experiments = this.getSatisfiedExperiments(data);
    const variants = [];
    for (const exp of experiments) {
      const v = this.decideVariant(
        exp.traffic_percentage,
        exp.variants,
        toss
      );
      if (v) {
        variants.push(v.id);
      }
    }
    return variants;
  }
  getSatisfiedExperiments(data) {
    return this.experiments.filter(
      (exp) => logic_default.apply(exp.context, data)
    );
  }
  // decide which variant to return among all applicable experiments
  decideVariant(traffic, applicable_variants, toss) {
    if (!Number.isInteger(traffic) || !Number.isInteger(toss)) {
      return void 0;
    }
    if (toss < 0) {
      for (const variant of applicable_variants) {
        if (variant.variant_type == "EXPERIMENTAL" /* EXPERIMENTAL */) {
          return variant;
        }
      }
    }
    const variant_count = applicable_variants.length;
    const range = traffic * variant_count;
    if (toss >= range) {
      return void 0;
    }
    const index = Math.floor(toss / traffic);
    return applicable_variants[index];
  }
};
export {
  CacReader,
  ExperimentReader
};
