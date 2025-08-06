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
        let mut class_names = Vec::new();
        let mut current_id = None;

        for attr in &elem.attrs {
            if let JSXAttrOrSpread::JSXAttr(attr) = attr {
                if let JSXAttrName::Ident(ident) = &attr.name {
                    match ident.sym.as_ref() {
                        "className" => {
                            if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr.value {
                                if !s.value.is_empty() {
                                    class_names = s.value.split_whitespace().map(String::from).collect();
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

        if !class_names.is_empty() || current_id.is_some() {
            self.elements.push(ElementInfo {
                span: elem.span,
                class_names,
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

pub fn determine_css_entities_and_updates(module: &Module) -> (HashSet<String>, HashSet<String>, HashMap<Span, String>) {
    let mut info_collector = InfoCollector { elements: Vec::new() };
    info_collector.visit_module(&module);

    let mut final_classnames = HashSet::new();
    let mut final_ids = HashSet::new();
    let mut id_updates = HashMap::new();
    
    let group_class_name = "group".to_string();

    let mut managed_elements_with_base_id = Vec::new();

    for el in info_collector.elements {
        final_classnames.extend(el.class_names.iter().cloned());

        if !el.class_names.contains(&group_class_name) {
            if let Some(id) = el.current_id {
                final_ids.insert(id);
            }
        } else {
            let non_group_classes: Vec<_> = el.class_names.iter().filter(|&cn| *cn != group_class_name).cloned().collect();
            let base_id = if non_group_classes.is_empty() {
                "G".to_string()
            } else {
                let classes_to_sample = if non_group_classes.len() > 5 {
                    vec![
                        non_group_classes[0].clone(),
                        non_group_classes[1].clone(),
                        non_group_classes[non_group_classes.len() / 2].clone(),
                        non_group_classes[non_group_classes.len() - 2].clone(),
                        non_group_classes[non_group_classes.len() - 1].clone(),
                    ]
                } else {
                    non_group_classes
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
