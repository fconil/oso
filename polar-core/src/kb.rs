use std::collections::HashMap;

use crate::error::ParameterError;
use crate::error::{PolarError, PolarResult};

pub use super::bindings::Bindings;
use super::counter::Counter;
use super::rules::*;
use super::sources::*;
use super::sugar::Namespaces;
use super::terms::*;
use std::sync::Arc;

enum RuleParamMatch {
    True,
    False(String),
}

impl RuleParamMatch {
    #[cfg(test)]
    fn is_true(&self) -> bool {
        matches!(self, RuleParamMatch::True)
    }
}

#[derive(Default)]
pub struct KnowledgeBase {
    /// A map of bindings: variable name → value. The VM uses a stack internally,
    /// but can translate to and from this type.
    pub constants: Bindings,
    /// Map of class name -> MRO list where the MRO list is a list of class instance IDs
    mro: HashMap<Symbol, Vec<u64>>,

    /// Map from loaded files to the source ID
    pub loaded_files: HashMap<String, u64>,
    /// Map from source code loaded to the filename it was loaded as
    pub loaded_content: HashMap<String, String>,

    rules: HashMap<Symbol, GenericRule>,
    rule_prototypes: HashMap<Symbol, Vec<Rule>>,
    pub sources: Sources,
    /// For symbols returned from gensym.
    gensym_counter: Counter,
    /// For call IDs, instance IDs, symbols, etc.
    id_counter: Counter,
    pub inline_queries: Vec<Term>,

    /// Namespace Bookkeeping
    pub namespaces: Namespaces,
}

impl KnowledgeBase {
    pub fn new() -> Self {
        Self {
            constants: HashMap::new(),
            mro: HashMap::new(),
            loaded_files: Default::default(),
            loaded_content: Default::default(),
            rules: HashMap::new(),
            rule_prototypes: HashMap::new(),
            sources: Sources::default(),
            id_counter: Counter::default(),
            gensym_counter: Counter::default(),
            inline_queries: vec![],
            namespaces: Namespaces::new(),
        }
    }

    /// Return a monotonically increasing integer ID.
    ///
    /// Wraps around at 52 bits of precision so that it can be safely
    /// coerced to an IEEE-754 double-float (f64).
    pub fn new_id(&self) -> u64 {
        self.id_counter.next()
    }

    pub fn id_counter(&self) -> Counter {
        self.id_counter.clone()
    }

    /// Generate a temporary variable prefix from a variable name.
    pub fn temp_prefix(name: &str) -> String {
        match name {
            "_" => String::from(name),
            _ => format!("_{}_", name),
        }
    }

    /// Generate a new symbol.
    pub fn gensym(&self, prefix: &str) -> Symbol {
        let next = self.gensym_counter.next();
        Symbol(format!("{}{}", Self::temp_prefix(prefix), next))
    }

    /// Add a generic rule to the knowledge base.
    #[cfg(test)]
    pub fn add_generic_rule(&mut self, rule: GenericRule) {
        self.rules.insert(rule.name.clone(), rule);
    }

    pub fn add_rule(&mut self, rule: Rule) {
        let generic_rule = self
            .rules
            .entry(rule.name.clone())
            .or_insert_with(|| GenericRule::new(rule.name.clone(), vec![]));
        generic_rule.add_rule(Arc::new(rule));
    }

    /// Validate that all rules loaded into the knowledge base are valid based on rule prototypes.
    pub fn validate_rules(&self) -> PolarResult<()> {
        for (rule_name, generic_rule) in &self.rules {
            if let Some(prototypes) = self.rule_prototypes.get(rule_name) {
                // If a prototype with the same name exists, then the parameters must match for each rule
                for rule in generic_rule.rules.values() {
                    let mut msg = "Must match one of the following rule prototypes:\n".to_owned();

                    let found_match = prototypes
                        .iter()
                        .map(|prototype| {
                            self.rule_params_match(rule.as_ref(), prototype)
                                .map(|result| (result, prototype))
                        })
                        .collect::<PolarResult<Vec<(RuleParamMatch, &Rule)>>>()
                        .map(|results| {
                            results.iter().any(|(result, prototype)| match result {
                                RuleParamMatch::True => true,
                                RuleParamMatch::False(message) => {
                                    msg.push_str(&format!(
                                        "\n{}\n\tFailed to match because: {}\n",
                                        prototype.to_polar(),
                                        message
                                    ));
                                    false
                                }
                            })
                        })?;
                    if !found_match {
                        return Err(self.set_error_context(
                            &rule.body,
                            error::ValidationError::InvalidRule {
                                rule: rule.to_polar(),
                                msg,
                            },
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Determine whether the fields of a rule parameter specializer match the fields of a prototype parameter specializer.
    /// Rule fields match if they are a superset of prototype fields and all field values are equal.
    // TODO: once field-level specializers are working this should be updated so
    // that it recursively checks all fields match, rather than checking for
    // equality
    fn param_fields_match(&self, prototype_fields: &Dictionary, rule_fields: &Dictionary) -> bool {
        return prototype_fields
            .fields
            .iter()
            .map(|(k, prototype_value)| {
                rule_fields
                    .fields
                    .get(k)
                    .map(|rule_value| rule_value == prototype_value)
                    .unwrap_or_else(|| false)
            })
            .all(|v| v);
    }

    /// Check that a rule parameter that has a pattern specializer matches a prototype parameter that has a pattern specializer.
    fn check_pattern_param(
        &self,
        index: usize,
        rule_pattern: &Pattern,
        prototype_pattern: &Pattern,
    ) -> PolarResult<RuleParamMatch> {
        Ok(match (prototype_pattern, rule_pattern) {
            (Pattern::Instance(prototype_instance), Pattern::Instance(rule_instance)) => {
                // if tags match, all prototype fields must match those in rule fields, otherwise false
                if prototype_instance.tag == rule_instance.tag {
                    if self.param_fields_match(
                        &prototype_instance.fields,
                        &rule_instance.fields,
                    ) {
                        RuleParamMatch::True
                    } else {
                        RuleParamMatch::False(format!("Rule specializer {} on parameter {} did not match prototype specializer {} because the specializer fields did not match.", rule_instance.to_polar(), index, prototype_instance.to_polar()))
                    }
                // If tags don't match, then rule specializer must be a subclass of prototype specializer
                } else if let Some(Value::ExternalInstance(ExternalInstance {
                    instance_id,
                    ..
                })) = self
                    .constants
                    .get(&prototype_instance.tag)
                    .map(|t| t.value())
                {
                    if let Some(rule_mro) = self.mro.get(&rule_instance.tag) {
                        if !rule_mro.contains(instance_id) {
                            RuleParamMatch::False(format!("Rule specializer {} on parameter {} must be a subclass of prototype specializer {}", rule_instance.tag,index, prototype_instance.tag))

                        } else if !self.param_fields_match(
                                &prototype_instance.fields,
                                &rule_instance.fields,
                            )
                        {
                            RuleParamMatch::False(format!("Rule specializer {} on parameter {} did not match prototype specializer {} because the specializer fields did not match.", rule_instance.to_polar(), index, prototype_instance.to_polar()))
                        } else {
                            RuleParamMatch::True
                        }
                    } else {
                        return Err(error::OperationalError::InvalidState(format!(
                                "All registered classes must have a registered MRO. Class {} does not have a registered MRO.",
                                &rule_instance.tag
                            )).into());
                    }
                } else {
                    unreachable!("Unregistered specializer classes should be caught before this point.");
                }
            }
            (Pattern::Dictionary(prototype_fields), Pattern::Dictionary(rule_fields))
            | (
                Pattern::Dictionary(prototype_fields),
                Pattern::Instance(InstanceLiteral {
                    tag: _,
                    fields: rule_fields,
                }),
            ) => {
                if self.param_fields_match(prototype_fields, rule_fields) {
                    RuleParamMatch::True
                } else {
                    RuleParamMatch::False(format!("Specializer mismatch on parameter {}. Rule specializer fields {:#?} do not match prototype specializer fields {:#?}.", index, rule_fields, prototype_fields))
                }
            }
            (
                Pattern::Instance(InstanceLiteral {
                    tag,
                    fields: prototype_fields,
                }),
                Pattern::Dictionary(rule_fields),
            ) if tag == &sym!("Dictionary") => {
                if self.param_fields_match(prototype_fields, rule_fields) {
                    RuleParamMatch::True
                } else {
                    RuleParamMatch::False(format!("Specializer mismatch on parameter {}. Rule specializer fields {:#?} do not match prototype specializer fields {:#?}.", index, rule_fields, prototype_fields))
                }
            }
            (_, _) => {
                RuleParamMatch::False(format!("Mismatch on parameter {}. Rule parameter {:#?} does not match prototype parameter {:#?}.", index, prototype_pattern, rule_pattern))
            }
        })
    }

    /// Check that a rule parameter that is a value matches a prototype parameter that is a value
    fn check_value_param(
        &self,
        index: usize,
        rule_value: &Value,
        prototype_value: &Value,
    ) -> PolarResult<RuleParamMatch> {
        Ok(match (prototype_value, rule_value) {
            (Value::List(prototype_list), Value::List(rule_list)) => {
                if prototype_list.iter().all(|t| rule_list.contains(t)) {
                    RuleParamMatch::True
                } else {
                    RuleParamMatch::False(format!(
                        "Invalid parameter {}. Rule prototype expected list {:#?}, got list {:#?}.",
                        index, prototype_list, rule_list
                    ))
                }
            }
            (Value::Dictionary(prototype_fields), Value::Dictionary(rule_fields)) => {
                if self.param_fields_match(prototype_fields, rule_fields) {
                    RuleParamMatch::True
                } else {
                    RuleParamMatch::False(format!("Invalid parameter {}. Rule prototype expected Dictionary with fields {:#?}, got Dictionary with fields {:#?}", index, prototype_fields, rule_fields
                        ))
                }
            }
            (_, _) => {
                if prototype_value == rule_value {
                    RuleParamMatch::True
                } else {
                    RuleParamMatch::False(format!(
                        "Invalid parameter {}. Rule value {} != prototype value {}",
                        index, rule_value, prototype_value
                    ))
                }
            }
        })
    }
    /// Check a single rule parameter against a prototype parameter.
    fn check_param(
        &self,
        index: usize,
        rule_param: &Parameter,
        prototype_param: &Parameter,
    ) -> PolarResult<RuleParamMatch> {
        Ok(
            match (
                prototype_param.parameter.value(),
                prototype_param.specializer.as_ref().map(Term::value),
                rule_param.parameter.value(),
                rule_param.specializer.as_ref().map(Term::value),
            ) {
                // Rule and prototype both have pattern specializers
                (
                    Value::Variable(_),
                    Some(Value::Pattern(prototype_spec)),
                    Value::Variable(_),
                    Some(Value::Pattern(rule_spec)),
                ) => self.check_pattern_param(index, rule_spec, prototype_spec)?,
                // Prototype has specializer but rule doesn't
                (Value::Variable(_), Some(prototype_spec), Value::Variable(_), None) => {
                    RuleParamMatch::False(format!(
                        "Invalid rule parameter {}. Rule prototype expected {}",
                        index,
                        prototype_spec.to_polar()
                    ))
                }
                // Rule has value or value specializer, prototype has pattern specializer
                (
                    Value::Variable(_),
                    Some(Value::Pattern(prototype_spec)),
                    Value::Variable(_),
                    Some(rule_value),
                )
                | (Value::Variable(_), Some(Value::Pattern(prototype_spec)), rule_value, None) => {
                    match prototype_spec {
                        // Prototype specializer is an instance pattern
                        Pattern::Instance(InstanceLiteral {
                            tag,
                            fields: prototype_fields,
                        }) => {
                            if match rule_value {
                                Value::String(_) => tag == &sym!("String"),
                                Value::Number(Numeric::Integer(_)) => tag == &sym!("Integer"),
                                Value::Number(Numeric::Float(_)) => tag == &sym!("Float"),
                                Value::Boolean(_) => tag == &sym!("Boolean"),
                                Value::List(_) => tag == &sym!("List"),
                                Value::Dictionary(rule_fields) => {
                                    tag == &sym!("Dictionary")
                                        && self.param_fields_match(prototype_fields, rule_fields)
                                }
                                _ => {
                                    unreachable!(
                                        "Value variant {} cannot be a specializer",
                                        rule_value
                                    )
                                }
                            } {
                                RuleParamMatch::True
                            } else {
                                RuleParamMatch::False(format!(
                                    "Invalid parameter {}. Rule prototype expected {}, got {}. ",
                                    index,
                                    tag.to_polar(),
                                    rule_value.to_polar()
                                ))
                            }
                        }
                        // Prototype specializer is a dictionary pattern
                        Pattern::Dictionary(prototype_fields) => {
                            if let Value::Dictionary(rule_fields) = rule_value {
                                if self.param_fields_match(prototype_fields, rule_fields) {
                                    RuleParamMatch::True
                                } else {
                                    RuleParamMatch::False(format!("Invalid parameter {}. Rule prototype expected Dictionary with fields {}, got dictionary with fields {}.", index, prototype_fields.to_polar(), rule_fields.to_polar()))
                                }
                            } else {
                                RuleParamMatch::False(format!("Invalid parameter {}. Rule prototype expected Dictionary, got {}.", index, rule_value.to_polar()))
                            }
                        }
                    }
                }

                // Prototype has no specializer
                (Value::Variable(_), None, _, _) => RuleParamMatch::True,
                // Rule has value or value specializer, prototype has value specializer |
                // rule has value, prototype has value
                (
                    Value::Variable(_),
                    Some(prototype_value),
                    Value::Variable(_),
                    Some(rule_value),
                )
                | (Value::Variable(_), Some(prototype_value), rule_value, None)
                | (prototype_value, None, rule_value, None) => {
                    self.check_value_param(index, rule_value, prototype_value)?
                }
                _ => RuleParamMatch::False(format!(
                    "Invalid parameter {}. Rule parameter {} does not match prototype parameter {}",
                    index,
                    rule_param.to_polar(),
                    prototype_param.to_polar()
                )),
            },
        )
    }

    /// Determine whether a rule matches a rule prototype based on its parameters.
    fn rule_params_match(&self, rule: &Rule, prototype: &Rule) -> PolarResult<RuleParamMatch> {
        if rule.params.len() != prototype.params.len() {
            return Ok(RuleParamMatch::False(format!(
                "Different number of parameters. Rule has {} parameter(s) but prototype has {}.",
                rule.params.len(),
                prototype.params.len()
            )));
        }
        let mut failure_message = "".to_owned();
        rule.params
            .iter()
            .zip(prototype.params.iter())
            .enumerate()
            .map(|(i, (rule_param, prototype_param))| {
                self.check_param(i + 1, rule_param, prototype_param)
            })
            .collect::<PolarResult<Vec<RuleParamMatch>>>()
            .map(|results| {
                results.iter().all(|r| {
                    if let RuleParamMatch::False(msg) = r {
                        failure_message = msg.to_owned();
                        false
                    } else {
                        true
                    }
                })
            })
            .map(|matched| {
                if matched {
                    RuleParamMatch::True
                } else {
                    RuleParamMatch::False(failure_message)
                }
            })
    }

    pub fn get_rules(&self) -> &HashMap<Symbol, GenericRule> {
        &self.rules
    }

    pub fn get_generic_rule(&self, name: &Symbol) -> Option<&GenericRule> {
        self.rules.get(name)
    }

    pub fn add_rule_prototype(&mut self, prototype: Rule) {
        let name = prototype.name.clone();
        // get rule prototypes
        let prototypes = self.rule_prototypes.entry(name).or_insert_with(Vec::new);
        prototypes.push(prototype);
    }

    /// Define a constant variable.
    pub fn constant(&mut self, name: Symbol, value: Term) {
        self.constants.insert(name, value);
    }

    /// Add the Method Resolution Order (MRO) list for a registered class.
    /// The `mro` argument is a list of the `instance_id` associated with a registered class.
    pub fn add_mro(&mut self, name: Symbol, mro: Vec<u64>) -> PolarResult<()> {
        // Confirm name is a registered class
        self.constants.get(&name).ok_or_else(|| {
            ParameterError(format!("Cannot add MRO for unregistered class {}", name))
        })?;
        self.mro.insert(name, mro);
        Ok(())
    }

    /// Return true if a constant with the given name has been defined.
    pub fn is_constant(&self, name: &Symbol) -> bool {
        self.constants.contains_key(name)
    }

    pub fn add_source(&mut self, source: Source) -> PolarResult<u64> {
        let src_id = self.new_id();
        if let Some(ref filename) = source.filename {
            self.check_file(&source.src, filename)?;
            self.loaded_content
                .insert(source.src.clone(), filename.to_string());
            self.loaded_files.insert(filename.to_string(), src_id);
        }
        self.sources.add_source(source, src_id);
        Ok(src_id)
    }

    pub fn clear_rules(&mut self) {
        self.rules.clear();
        self.rule_prototypes.clear();
        self.sources = Sources::default();
        self.inline_queries.clear();
        self.loaded_content.clear();
        self.loaded_files.clear();
    }

    /// Removes a file from the knowledge base by finding the associated
    /// `Source` and removing all rules for that source, and
    /// removes the file from loaded files.
    ///
    /// Optionally return the source for the file, returning `None`
    /// if the file was not in the loaded files.
    pub fn remove_file(&mut self, filename: &str) -> Option<String> {
        self.loaded_files
            .get(filename)
            .cloned()
            .map(|src_id| self.remove_source(src_id))
    }

    /// Removes a source from the knowledge base by finding the associated
    /// `Source` and removing all rules for that source. Will
    /// also remove the loaded files if the source was loaded from a file.
    pub fn remove_source(&mut self, source_id: u64) -> String {
        // remove from rules
        self.rules.retain(|_, gr| {
            let to_remove: Vec<u64> = gr.rules.iter().filter_map(|(idx, rule)| {
                if matches!(rule.source_info, SourceInfo::Parser { src_id, ..} if src_id == source_id) {
                    Some(*idx)
                } else {
                    None
                }
            }).collect();

            for idx in to_remove {
                gr.remove_rule(idx);
            }
            !gr.rules.is_empty()
        });

        // remove from sources
        let source = self
            .sources
            .remove_source(source_id)
            .expect("source doesn't exist in KB");
        let filename = source.filename;

        // remove queries
        self.inline_queries
            .retain(|q| q.get_source_id() != Some(source_id));

        // remove from files
        if let Some(filename) = filename {
            self.loaded_files.remove(&filename);
            self.loaded_content.retain(|_, f| f != &filename);
        }
        source.src
    }

    fn check_file(&self, src: &str, filename: &str) -> PolarResult<()> {
        match (
            self.loaded_content.get(src),
            self.loaded_files.get(filename).is_some(),
        ) {
            (Some(other_file), true) if other_file == filename => {
                return Err(error::RuntimeError::FileLoading {
                    msg: format!("File {} has already been loaded.", filename),
                }
                .into())
            }
            (_, true) => {
                return Err(error::RuntimeError::FileLoading {
                    msg: format!(
                        "A file with the name {}, but different contents has already been loaded.",
                        filename
                    ),
                }
                .into());
            }
            (Some(other_file), _) => {
                return Err(error::RuntimeError::FileLoading {
                    msg: format!(
                        "A file with the same contents as {} named {} has already been loaded.",
                        filename, other_file
                    ),
                }
                .into());
            }
            _ => {}
        }
        Ok(())
    }

    pub fn set_error_context(&self, term: &Term, error: impl Into<PolarError>) -> PolarError {
        let source = term
            .get_source_id()
            .and_then(|id| self.sources.get_source(id));
        let error: PolarError = error.into();
        error.set_context(source.as_ref(), Some(term))
    }

    pub fn rewrite_implications(&mut self) -> PolarResult<()> {
        let mut errors = vec![];

        errors.append(&mut super::sugar::check_all_relation_types_have_been_registered(self));

        // TODO(gj): Emit all errors instead of just the first.
        if !errors.is_empty() {
            self.namespaces.clear();
            return Err(errors[0].clone());
        }

        let mut rules = vec![];
        for (namespace, implications) in &self.namespaces.implications {
            for implication in implications {
                match implication.as_rule(namespace, &self.namespaces) {
                    Ok(rule) => rules.push(rule),
                    Err(error) => errors.push(error),
                }
            }
        }

        // If we've reached this point, we're all done with the namespaces.
        self.namespaces.clear();

        // TODO(gj): Emit all errors instead of just the first.
        if !errors.is_empty() {
            return Err(errors[0].clone());
        }

        // Add the rewritten rules to the KB.
        for rule in rules {
            self.add_rule(rule);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::*;
    #[test]
    fn test_rule_params_match() {
        let mut kb = KnowledgeBase::new();
        kb.constant(
            sym!("Fruit"),
            term!(Value::ExternalInstance(ExternalInstance {
                instance_id: 1,
                constructor: None,
                repr: None
            })),
        );
        kb.constant(
            sym!("Citrus"),
            term!(Value::ExternalInstance(ExternalInstance {
                instance_id: 2,
                constructor: None,
                repr: None
            })),
        );
        kb.constant(
            sym!("Orange"),
            term!(Value::ExternalInstance(ExternalInstance {
                instance_id: 3,
                constructor: None,
                repr: None
            })),
        );
        kb.add_mro(sym!("Fruit"), vec![1]).unwrap();
        // Citrus is a subclass of Fruit
        kb.add_mro(sym!("Citrus"), vec![2, 1]).unwrap();
        // Orange is a subclass of Citrus
        kb.add_mro(sym!("Orange"), vec![3, 2, 1]).unwrap();

        // BOTH PATTERN SPEC
        // rule: f(x: Foo), prototype: f(x: Foo) => PASS
        assert!(kb
            .rule_params_match(
                &rule!("f", ["x"; instance!(sym!("Fruit"))]),
                &rule!("f", ["x"; instance!(sym!("Fruit"))])
            )
            .unwrap()
            .is_true());

        // rule: f(x: Foo), prototype: f(x: Bar) => FAIL if Foo is not subclass of Bar
        assert!(!kb
            .rule_params_match(
                &rule!("f", ["x"; instance!(sym!("Fruit"))]),
                &rule!("f", ["x"; instance!(sym!("Citrus"))])
            )
            .unwrap()
            .is_true());

        // rule: f(x: Foo), prototype: f(x: Bar) => PASS if Foo is subclass of Bar
        assert!(kb
            .rule_params_match(
                &rule!("f", ["x"; instance!(sym!("Citrus"))]),
                &rule!("f", ["x"; instance!(sym!("Fruit"))])
            )
            .unwrap()
            .is_true());
        assert!(kb
            .rule_params_match(
                &rule!("f", ["x"; instance!(sym!("Orange"))]),
                &rule!("f", ["x"; instance!(sym!("Fruit"))])
            )
            .unwrap()
            .is_true());

        // rule: f(x: Foo), prototype: f(x: {id: 1}) => FAIL
        assert!(!kb
            .rule_params_match(
                &rule!("f", ["x"; instance!(sym!("Foo"))]),
                &rule!("f", ["x"; btreemap! {sym!("id") => term!(1)}])
            )
            .unwrap()
            .is_true());
        // rule: f(x: Foo{id: 1}), prototype: f(x: {id: 1}) => PASS
        assert!(kb
            .rule_params_match(
                &rule!(
                    "f",
                    ["x"; instance!(sym!("Foo"), btreemap! {sym!("id") => term!(1)})]
                ),
                &rule!("f", ["x"; btreemap! {sym!("id") => term!(1)}])
            )
            .unwrap()
            .is_true());
        // rule: f(x: {id: 1}), prototype: f(x: Foo{id: 1}) => FAIL
        assert!(!kb
            .rule_params_match(
                &rule!("f", ["x"; btreemap! {sym!("id") => term!(1)}]),
                &rule!(
                    "f",
                    ["x"; instance!(sym!("Foo"), btreemap! {sym!("id") => term!(1)})]
                )
            )
            .unwrap()
            .is_true());
        // rule: f(x: {id: 1}), prototype: f(x: {id: 1}) => PASS
        assert!(kb
            .rule_params_match(
                &rule!("f", ["x"; btreemap! {sym!("id") => term!(1)}]),
                &rule!("f", ["x"; btreemap! {sym!("id") => term!(1)}])
            )
            .unwrap()
            .is_true());

        // RULE VALUE SPEC, TEMPLATE PATTERN SPEC
        // rule: f(x: 6), prototype: f(x: Integer) => PASS
        assert!(kb
            .rule_params_match(
                &rule!("f", ["x"; value!(6)]),
                &rule!("f", ["x"; instance!(sym!("Integer"))])
            )
            .unwrap()
            .is_true());
        // rule: f(x: 6), prototype: f(x: Foo) => FAIL
        assert!(!kb
            .rule_params_match(
                &rule!("f", ["x"; value!(6)]),
                &rule!("f", ["x"; instance!(sym!("Foo"))])
            )
            .unwrap()
            .is_true());
        // rule: f(x: 6.0), prototype: f(x: Float) => PASS
        assert!(kb
            .rule_params_match(
                &rule!("f", ["x"; value!(6.0)]),
                &rule!("f", ["x"; instance!(sym!("Float"))])
            )
            .unwrap()
            .is_true());
        // rule: f(x: 6.0), prototype: f(x: Foo) => FAIL
        assert!(!kb
            .rule_params_match(
                &rule!("f", ["x"; value!(6.0)]),
                &rule!("f", ["x"; instance!(sym!("Foo"))])
            )
            .unwrap()
            .is_true());
        // rule: f(x: "hi"), prototype: f(x: String) => PASS
        assert!(kb
            .rule_params_match(
                &rule!("f", ["x"; value!("hi")]),
                &rule!("f", ["x"; instance!(sym!("String"))])
            )
            .unwrap()
            .is_true());
        // rule: f(x: "hi"), prototype: f(x: Foo) => FAIL
        assert!(!kb
            .rule_params_match(
                &rule!("f", ["x"; value!("hi")]),
                &rule!("f", ["x"; instance!(sym!("Foo"))])
            )
            .unwrap()
            .is_true());
        // rule: f(x: true), prototype: f(x: Boolean) => PASS
        assert!(kb
            .rule_params_match(
                &rule!("f", ["x"; value!(true)]),
                &rule!("f", ["x"; instance!(sym!("Boolean"))])
            )
            .unwrap()
            .is_true());
        // rule: f(x: true), prototype: f(x: Foo) => FAIL
        assert!(!kb
            .rule_params_match(
                &rule!("f", ["x"; value!(true)]),
                &rule!("f", ["x"; instance!(sym!("Foo"))])
            )
            .unwrap()
            .is_true());
        // rule: f(x: [1, 2]), prototype: f(x: List) => PASS
        assert!(kb
            .rule_params_match(
                &rule!("f", ["x"; value!([1, 2])]),
                &rule!("f", ["x"; instance!(sym!("List"))])
            )
            .unwrap()
            .is_true());
        // rule: f(x: [1, 2]), prototype: f(x: Foo) => FAIL
        assert!(!kb
            .rule_params_match(
                &rule!("f", ["x"; value!([1, 2])]),
                &rule!("f", ["x"; instance!(sym!("Foo"))])
            )
            .unwrap()
            .is_true());
        // rule: f(x: {id: 1}), prototype: f(x: Dictionary) => PASS
        assert!(kb
            .rule_params_match(
                &rule!("f", ["x"; btreemap! {sym!("id") => term!(1)}]),
                &rule!("f", ["x"; instance!(sym!("Dictionary"))])
            )
            .unwrap()
            .is_true());
        // rule: f(x: {id: 1}), prototype: f(x: Foo) => FAIL
        assert!(!kb
            .rule_params_match(
                &rule!("f", ["x"; btreemap! {sym!("id") => term!(1)}]),
                &rule!("f", ["x"; instance!(sym!("Foo"))])
            )
            .unwrap()
            .is_true());
        // rule: f(x: {id: 1}), prototype: f(x: Dictionary{id: 1}) => PASS
        assert!(kb
            .rule_params_match(
                &rule!("f", ["x"; btreemap! {sym!("id") => term!(1)}]),
                &rule!(
                    "f",
                    ["x"; instance!(sym!("Dictionary"), btreemap! {sym!("id") => term!(1)})]
                )
            )
            .unwrap()
            .is_true());

        // RULE PATTERN SPEC, TEMPLATE VALUE SPEC
        // always => FAIL
        assert!(!kb
            .rule_params_match(
                &rule!("f", ["x"; btreemap!(sym!("1") => term!(1))]),
                &rule!("f", ["x"; value!(1)])
            )
            .unwrap()
            .is_true());

        // BOTH VALUE SPEC
        // Integer, String, Boolean: must be equal
        // rule: f(x: 1), prototype: f(x: 1) => PASS
        assert!(kb
            .rule_params_match(&rule!("f", ["x"; value!(1)]), &rule!("f", ["x"; value!(1)]))
            .unwrap()
            .is_true());
        // rule: f(x: 1), prototype: f(x: 2) => FAIL
        assert!(!kb
            .rule_params_match(&rule!("f", ["x"; value!(1)]), &rule!("f", ["x"; value!(2)]))
            .unwrap()
            .is_true());
        // rule: f(x: 1.0), prototype: f(x: 1.0) => PASS
        assert!(kb
            .rule_params_match(
                &rule!("f", ["x"; value!(1.0)]),
                &rule!("f", ["x"; value!(1.0)])
            )
            .unwrap()
            .is_true());
        // rule: f(x: 1.0), prototype: f(x: 2.0) => FAIL
        assert!(!kb
            .rule_params_match(
                &rule!("f", ["x"; value!(1.0)]),
                &rule!("f", ["x"; value!(2.0)])
            )
            .unwrap()
            .is_true());
        // rule: f(x: "hi"), prototype: f(x: "hi") => PASS
        assert!(kb
            .rule_params_match(
                &rule!("f", ["x"; value!("hi")]),
                &rule!("f", ["x"; value!("hi")])
            )
            .unwrap()
            .is_true());
        // rule: f(x: "hi"), prototype: f(x: "hello") => FAIL
        assert!(!kb
            .rule_params_match(
                &rule!("f", ["x"; value!("hi")]),
                &rule!("f", ["x"; value!("hello")])
            )
            .unwrap()
            .is_true());
        // rule: f(x: true), prototype: f(x: true) => PASS
        assert!(kb
            .rule_params_match(
                &rule!("f", ["x"; value!(true)]),
                &rule!("f", ["x"; value!(true)])
            )
            .unwrap()
            .is_true());
        // rule: f(x: true), prototype: f(x: false) => PASS
        assert!(!kb
            .rule_params_match(
                &rule!("f", ["x"; value!(true)]),
                &rule!("f", ["x"; value!(false)])
            )
            .unwrap()
            .is_true());
        // List: rule must be more specific than (superset of) prototype
        // rule: f(x: [1,2,3]), prototype: f(x: [1,2]) => PASS
        // TODO: I'm not sure this logic actually makes sense--it feels like
        // they should have to be an exact match
        assert!(kb
            .rule_params_match(
                &rule!("f", ["x"; value!([1, 2, 3])]),
                &rule!("f", ["x"; value!([1, 2])])
            )
            .unwrap()
            .is_true());
        // rule: f(x: [1,2]), prototype: f(x: [1,2,3]) => FAIL
        assert!(!kb
            .rule_params_match(
                &rule!("f", ["x"; value!([1, 2])]),
                &rule!("f", ["x"; value!([1, 2, 3])])
            )
            .unwrap()
            .is_true());
        // Dict: rule must be more specific than (superset of) prototype
        // rule: f(x: {"id": 1, "name": "Dave"}), prototype: f(x: {"id": 1}) => PASS
        assert!(kb
            .rule_params_match(
                &rule!(
                    "f",
                    ["x"; btreemap! {sym!("id") => term!(1), sym!("name") => term!(sym!("Dave"))}]
                ),
                &rule!("f", ["x"; btreemap! {sym!("id") => term!(1)}]),
            )
            .unwrap()
            .is_true());
        // rule: f(x: {"id": 1}), prototype: f(x: {"id": 1, "name": "Dave"}) => FAIL
        assert!(!kb
            .rule_params_match(
                &rule!("f", ["x"; btreemap! {sym!("id") => term!(1)}]),
                &rule!(
                    "f",
                    ["x"; btreemap! {sym!("id") => term!(1), sym!("name") => term!(sym!("Dave"))}]
                )
            )
            .unwrap()
            .is_true());

        // RULE None SPEC TEMPLATE Some SPEC
        // always => FAIL
        assert!(!kb
            .rule_params_match(
                &rule!("f", [sym!("x")]),
                &rule!("f", ["x"; instance!(sym!("Foo"))])
            )
            .unwrap()
            .is_true());

        // RULE Some SPEC TEMPLATE None SPEC
        // always => PASS
        assert!(kb
            .rule_params_match(
                &rule!("f", ["x"; instance!(sym!("Foo"))]),
                &rule!("f", [sym!("x")]),
            )
            .unwrap()
            .is_true());
    }

    #[test]
    fn test_validate_rules() {
        let mut kb = KnowledgeBase::new();
        kb.constant(
            sym!("Fruit"),
            term!(Value::ExternalInstance(ExternalInstance {
                instance_id: 1,
                constructor: None,
                repr: None
            })),
        );
        kb.constant(
            sym!("Citrus"),
            term!(Value::ExternalInstance(ExternalInstance {
                instance_id: 2,
                constructor: None,
                repr: None
            })),
        );
        kb.constant(
            sym!("Orange"),
            term!(Value::ExternalInstance(ExternalInstance {
                instance_id: 3,
                constructor: None,
                repr: None
            })),
        );
        kb.add_mro(sym!("Fruit"), vec![1]).unwrap();
        // Citrus is a subclass of Fruit
        kb.add_mro(sym!("Citrus"), vec![2, 1]).unwrap();
        // Orange is a subclass of Citrus
        kb.add_mro(sym!("Orange"), vec![3, 2, 1]).unwrap();

        // Prototype applies if it has the same name as a rule
        kb.add_rule_prototype(rule!("f", ["x"; instance!(sym!("Orange"))]));
        kb.add_rule(rule!("f", ["x"; instance!(sym!("Orange"))]));
        kb.add_rule(rule!("f", ["x"; instance!(sym!("Fruit"))]));

        assert!(matches!(
            kb.validate_rules().err().unwrap(),
            PolarError {
                kind: ErrorKind::Validation(ValidationError::InvalidRule { .. }),
                ..
            }
        ));

        // Prototype does not apply if it doesn't have the same name as a rule
        kb.clear_rules();
        kb.add_rule_prototype(rule!("f", ["x"; instance!(sym!("Orange"))]));
        kb.add_rule(rule!("f", ["x"; instance!(sym!("Orange"))]));
        kb.add_rule(rule!("g", ["x"; instance!(sym!("Fruit"))]));

        kb.validate_rules().unwrap();

        // Prototype does apply if it has the same name as a rule even if different arity
        kb.clear_rules();
        kb.add_rule_prototype(rule!("f", ["x"; instance!(sym!("Orange")), value!(1)]));
        kb.add_rule(rule!("f", ["x"; instance!(sym!("Orange"))]));

        assert!(matches!(
            kb.validate_rules().err().unwrap(),
            PolarError {
                kind: ErrorKind::Validation(ValidationError::InvalidRule { .. }),
                ..
            }
        ));
        // Multiple templates can exist for the same name but only one needs to match
        kb.clear_rules();
        kb.add_rule_prototype(rule!("f", ["x"; instance!(sym!("Orange"))]));
        kb.add_rule_prototype(rule!("f", ["x"; instance!(sym!("Orange")), value!(1)]));
        kb.add_rule_prototype(rule!("f", ["x"; instance!(sym!("Fruit"))]));
        kb.add_rule(rule!("f", ["x"; instance!(sym!("Fruit"))]));
    }
}
