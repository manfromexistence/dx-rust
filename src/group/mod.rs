use regex::{Captures, Regex};
use std::collections::HashMap;
use swc_common::{Span, SyntaxContext};
use swc_ecma_ast::{
    Module, VarDecl, VarDeclarator, Pat, Lit, Expr, JSXAttr, JSXAttrName, JSXAttrValue,
    Ident, Stmt, Decl, ModuleItem,
};
use swc_ecma_visit::{VisitMut, VisitMutWith};

pub struct GroupTransformer {
    serializer_count: u32,
    pub new_vars: Vec<VarDecl>,
    pub resolved_classes: HashMap<Span, Vec<String>>,
}

impl GroupTransformer {
    pub fn new() -> Self {
        GroupTransformer {
            serializer_count: 0,
            new_vars: Vec::new(),
            resolved_classes: HashMap::new(),
        }
    }

    fn get_abbreviated(&self, classes_str: &str) -> String {
        let classes: Vec<_> = classes_str.split('+').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
        if classes.is_empty() { return "".to_string(); }

        let classes_to_sample = if classes.len() > 5 {
            vec![
                classes[0],
                classes[1],
                classes[classes.len() / 2],
                classes[classes.len() - 2],
                classes[classes.len() - 1],
            ]
        } else {
            classes
        };

        let mut id_chars: Vec<char> = classes_to_sample
            .iter()
            .filter_map(|s| s.chars().next())
            .map(|c| c.to_ascii_uppercase())
            .collect();
        
        id_chars.sort_unstable();
        id_chars.dedup();
        id_chars.into_iter().collect()
    }
}

impl VisitMut for GroupTransformer {
    fn visit_mut_jsx_attr(&mut self, attr: &mut JSXAttr) {
        if let JSXAttrName::Ident(ident) = &attr.name {
            if ident.sym == "className" {
                if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &mut attr.value {
                    let original_value = s.value.to_string();
                    let re = Regex::new(r"(\w*)\(([^)]+)\)").unwrap();
                    
                    if re.is_match(&original_value) {
                        let mut full_class_list = Vec::new();
                        let mut var_name = String::new();

                        let transformed_str = re.replace(&original_value, |caps: &Captures| {
                            let prefix = caps.get(1).map_or("", |m| m.as_str());
                            let classes_part = caps.get(2).map_or("", |m| m.as_str()).trim_end_matches('+');
                            
                            var_name = if prefix.is_empty() {
                                self.serializer_count += 1;
                                format!("_{}", self.serializer_count)
                            } else {
                                prefix.to_string()
                            };

                            let classes_in_group: Vec<_> = classes_part.split('+').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
                            full_class_list.extend(classes_in_group.iter().map(|s| s.to_string()));

                            let var_value = classes_in_group.join(" ");
                            let abbreviated = self.get_abbreviated(classes_part);

                            let new_var_decl = VarDecl {
                                span: Default::default(),
                                kind: swc_ecma_ast::VarDeclKind::Let,
                                declare: false,
                                ctxt: Default::default(),
                                decls: vec![VarDeclarator {
                                    span: Default::default(),
                                    name: Pat::Ident(Ident::new(var_name.clone().into(), Default::default(), Default::default()).into()),
                                    init: Some(Box::new(Expr::Lit(Lit::Str(swc_ecma_ast::Str {
                                        span: Default::default(),
                                        value: var_value.into(),
                                        raw: None,
                                    })))),
                                    definite: false,
                                }],
                            };
                            self.new_vars.push(new_var_decl);
                            
                            format!("{}({}+)", var_name, abbreviated)
                        }).to_string();

                        let remaining_classes: Vec<_> = re.replace_all(&original_value, "").split_whitespace().map(String::from).collect();
                        full_class_list.extend(remaining_classes);
                        self.resolved_classes.insert(attr.span, full_class_list);

                        attr.value = Some(JSXAttrValue::Lit(Lit::Str(swc_ecma_ast::Str {
                            value: transformed_str.into(),
                            span: s.span,
                            raw: None,
                        })));
                    }
                }
            }
        }
        attr.visit_mut_children_with(self);
    }

    fn visit_mut_module(&mut self, module: &mut Module) {
        module.visit_mut_children_with(self);

        if !self.new_vars.is_empty() {
            let new_items: Vec<ModuleItem> = self.new_vars.drain(..).map(|var_decl| ModuleItem::Stmt(Stmt::Decl(Decl::Var(Box::new(var_decl))))).collect();
            module.body.splice(0..0, new_items);
        }
    }
}
