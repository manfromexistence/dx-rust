use swc_common::{sync::Lrc, SourceMap};
use swc_ecma_ast::{JSXAttrName, JSXAttrOrSpread, JSXAttrValue, JSXOpeningElement, Lit, Module};
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_visit::{Visit, VisitMut, VisitWith};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn process_tsx(input: &str) -> JsValue {
    let cm: Lrc<SourceMap> = Default::default();
    let fm = cm.new_source_file(
        swc_common::FileName::Custom("input.tsx".to_string()),
        input.to_string(),
    );
    let lexer = Lexer::new(
        Syntax::Typescript(TsSyntax {
            tsx: true,
            ..Default::default()
        }),
        Default::default(),
        StringInput::from(&*fm),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let mut module = match parser.parse_module() {
        Ok(module) => module,
        Err(_) => return JsValue::from_serde(&Vec::<String>::new()).unwrap(),
    };
    let mut pruner = JSXPruner;
    module.visit_mut_with(&mut pruner);
    let mut classnames = Vec::new();
    let mut collector = JSXOnlyCollector {
        classnames: &mut classnames,
    };
    module.visit_with(&mut collector);
    JsValue::from_serde(&classnames).unwrap()
}

struct JSXOnlyCollector<'a> {
    classnames: &'a mut Vec<String>,
}

impl<'a> Visit for JSXOnlyCollector<'a> {
    fn visit_jsx_opening_element(&mut self, elem: &JSXOpeningElement) {
        let attrs = &elem.attrs;
        let mut i = 0;
        while i < attrs.len() {
            if let (Some(JSXAttrOrSpread::JSXAttr(attr1)), Some(JSXAttrOrSpread::JSXAttr(attr2))) = (attrs.get(i), attrs.get(i + 1)) {
                if let JSXAttrName::Ident(ident) = &attr1.name {
                    if ident.sym == "className" {
                        if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr1.value {
                            self.classnames.extend(s.value.split_whitespace().map(String::from));
                        }
                    }
                }
                if let JSXAttrName::Ident(ident) = &attr2.name {
                    if ident.sym == "className" {
                        if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr2.value {
                            self.classnames.extend(s.value.split_whitespace().map(String::from));
                        }
                    }
                }
                i += 2;
            } else if let Some(JSXAttrOrSpread::JSXAttr(attr)) = attrs.get(i) {
                if let JSXAttrName::Ident(ident) = &attr.name {
                    if ident.sym == "className" {
                        if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr.value {
                            self.classnames.extend(s.value.split_whitespace().map(String::from));
                        }
                    }
                }
                i += 1;
            } else {
                i += 1;
            }
        }
    }
}

struct JSXPruner;

impl VisitMut for JSXPruner {
    fn visit_mut_module(&mut self, module: &mut Module) {
        module.body.retain(|item| matches!(item, swc_ecma_ast::ModuleItem::Stmt(swc_ecma_ast::Stmt::Decl(swc_ecma_ast::Decl::TsInterface(_))) || item.is_module_decl());
        for item in &mut module.body {
            if let swc_ecma_ast::ModuleItem::Stmt(swc_ecma_ast::Stmt::Expr(expr)) = item {
                expr.visit_mut_children_with(self);
            }
        }
    }

    fn visit_mut_jsx_element(&mut self, elem: &mut swc_ecma_ast::JSXElement) {
        elem.children.retain(|child| matches!(child, swc_ecma_ast::JSXElementChild::JSXElement(_)));
        for child in &mut elem.children {
            child.visit_mut_children_with(self);
        }
    }
}