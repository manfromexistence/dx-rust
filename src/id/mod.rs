use std::collections::{BTreeMap, HashMap, HashSet};
use swc_common::{Span};
use swc_ecma_ast::{
    IdentName, JSXAttr, JSXAttrName, JSXAttrOrSpread, JSXAttrValue, JSXOpeningElement, Lit, Str, Module,
};
use swc_ecma_visit::{Visit, VisitMut, VisitWith, VisitMutWith};

#[derive(Debug, Clone)]
pub struct ElementInfo {
    pub span: Span,
    pub class_names: Vec<String>,
    pub current_id: Option<String>,
}

pub struct InfoCollector {
    pub elements: Vec<ElementInfo>,
}

impl Visit for InfoCollector {
    fn visit_jsx_opening_element(&mut self, elem: &JSXOpeningElement) {
        let mut all_class_names = Vec::new();
        let mut current_id = None;

        for attr in &elem.attrs {
            if let JSXAttrOrSpread::JSXAttr(attr) = attr {
                if let JSXAttrName::Ident(ident) = &attr.name {
                    match ident.sym.as_ref() {
                        "className" => {
                            if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr.value {
                                if !s.value.is_empty() {
                                    all_class_names.extend(s.value.split_whitespace().map(String::from));
                                }
                            }
                        }
                        "id" => {
                            if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr.value {
                                if !s.value.is_empty() {
                                    current_id = Some(s.value.to_string());
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        
        all_class_names.sort();
        all_class_names.dedup();

        if !all_class_names.is_empty() || current_id.is_some() {
            self.elements.push(ElementInfo {
                span: elem.span,
                class_names: all_class_names,
                current_id,
            });
        }
        
        elem.visit_children_with(self);
    }
}

pub struct IdApplier<'a> {
    pub id_map: &'a HashMap<Span, String>,
}

impl<'a> VisitMut for IdApplier<'a> {
    fn visit_mut_jsx_opening_element(&mut self, elem: &mut JSXOpeningElement) {
        if let Some(new_id) = self.id_map.get(&elem.span) {
            let mut has_id_attr = false;
            for attr in &mut elem.attrs {
                if let JSXAttrOrSpread::JSXAttr(jsx_attr) = attr {
                    if let JSXAttrName::Ident(ident) = &jsx_attr.name {
                        if ident.sym == "id" {
                            jsx_attr.value = Some(JSXAttrValue::Lit(Lit::Str(Str {
                                value: new_id.clone().into(),
                                span: Default::default(),
                                raw: None,
                            })));
                            has_id_attr = true;
                            break;
                        }
                    }
                }
            }

            if !has_id_attr {
                elem.attrs.push(JSXAttrOrSpread::JSXAttr(JSXAttr {
                    name: JSXAttrName::Ident(IdentName::new("id".into(), Default::default())),
                    value: Some(JSXAttrValue::Lit(Lit::Str(Str {
                        value: new_id.clone().into(),
                        span: Default::default(),
                        raw: None,
                    }))),
                    span: Default::default(),
                }));
            }
        }
        elem.visit_mut_children_with(self);
    }
}

pub fn determine_css_entities_and_updates(module: &Module, resolved_classes: &HashMap<Span, Vec<String>>) -> (HashSet<String>, HashSet<String>, HashMap<Span, String>) {
    let mut info_collector = InfoCollector { elements: Vec::new() };
    info_collector.visit_module(&module);

    let mut final_classnames = HashSet::new();
    let mut final_ids = HashSet::new();
    let mut id_updates = HashMap::new();
    
    let id_trigger_class = "id".to_string();

    let mut managed_elements_with_base_id = Vec::new();

    for el in info_collector.elements {
        let classes_for_id = resolved_classes.get(&el.span).unwrap_or(&el.class_names);
        final_classnames.extend(classes_for_id.iter().cloned());

        if !classes_for_id.contains(&id_trigger_class) {
            if let Some(id) = el.current_id {
                final_ids.insert(id);
            }
        } else {
            let non_trigger_classes: Vec<_> = classes_for_id.iter().filter(|&cn| *cn != id_trigger_class).cloned().collect();
            let base_id = if non_trigger_classes.is_empty() {
                "G".to_string()
            } else {
                let classes_to_sample = if non_trigger_classes.len() > 5 {
                    vec![
                        non_trigger_classes[0].clone(),
                        non_trigger_classes[1].clone(),
                        non_trigger_classes[non_trigger_classes.len() / 2].clone(),
                        non_trigger_classes[non_trigger_classes.len() - 2].clone(),
                        non_trigger_classes[non_trigger_classes.len() - 1].clone(),
                    ]
                } else {
                    non_trigger_classes
                };
                
                let mut id_chars: Vec<char> = classes_to_sample
                    .iter()
                    .filter_map(|s| s.chars().next())
                    .map(|c| c.to_ascii_uppercase())
                    .collect();
                
                id_chars.sort_unstable();
                id_chars.dedup();
                id_chars.into_iter().collect()
            };
            managed_elements_with_base_id.push((base_id, el));
        }
    }

    let mut elements_by_base_id: BTreeMap<String, Vec<ElementInfo>> = BTreeMap::new();
    for (base_id, el_info) in managed_elements_with_base_id {
        elements_by_base_id.entry(base_id).or_insert_with(Vec::new).push(el_info);
    }
    
    for (base_id, elements) in elements_by_base_id {
        if elements.len() > 1 {
            for (i, el) in elements.iter().enumerate() {
                let final_id = format!("{}{}", base_id, i + 1);
                if el.current_id.as_deref() != Some(&final_id) {
                    id_updates.insert(el.span, final_id.clone());
                }
                final_ids.insert(final_id);
            }
        } else if let Some(el) = elements.first() {
            let final_id = base_id.clone();
            if el.current_id.as_deref() != Some(&final_id) {
                id_updates.insert(el.span, final_id.clone());
            }
            final_ids.insert(final_id);
        }
    }
    
    (final_classnames, final_ids, id_updates)
}
