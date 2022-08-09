use std::collections::HashSet;

use crate::expression_analyzer;
use crate::scope_context::ScopeContext;
use crate::statements_analyzer::StatementsAnalyzer;
use crate::typed_ast::TastInfo;
use hakana_reflection_info::{
    data_flow::{node::DataFlowNode, path::PathKind},
    t_atomic::TAtomic,
    t_union::TUnion,
    taint::TaintType,
};
use hakana_type::{get_mixed_any, type_combiner};
use oxidized::{aast, ast, ast_defs::Pos};

pub(crate) fn analyze<'expr: 'tast, 'map, 'new_expr, 'tast>(
    statements_analyzer: &StatementsAnalyzer,
    stmt_pos: &aast::Pos,
    operator: &'expr ast::Bop,
    left: &'expr aast::Expr<(), ()>,
    right: &'expr aast::Expr<(), ()>,
    tast_info: &'tast mut TastInfo,
    context: &mut ScopeContext,
) {
    expression_analyzer::analyze(statements_analyzer, left, tast_info, context, &mut None);
    expression_analyzer::analyze(statements_analyzer, right, tast_info, context, &mut None);

    let fallback = get_mixed_any();
    let e1_type = match tast_info.get_expr_type(&left.1) {
        Some(var_type) => var_type,
        None => &fallback,
    };

    let e2_type = match tast_info.get_expr_type(&right.1) {
        Some(var_type) => var_type,
        None => &fallback,
    };

    let zero = TAtomic::TLiteralInt { value: 0 };

    let mut results = vec![];

    for (_, mut e1_type_atomic) in &e1_type.types {
        if let TAtomic::TFalse = e1_type_atomic {
            if e1_type.ignore_falsable_issues {
                continue;
            }
            e1_type_atomic = &zero;
        }
        for (_, mut e2_type_atomic) in &e2_type.types {
            if let TAtomic::TFalse = e2_type_atomic {
                if e2_type.ignore_falsable_issues {
                    continue;
                }
                e2_type_atomic = &zero;
            }

            results.push(match (e1_type_atomic, e2_type_atomic) {
                (
                    TAtomic::TLiteralInt { value: e1_value },
                    TAtomic::TLiteralInt { value: e2_value },
                ) => match operator {
                    oxidized::ast_defs::Bop::Plus => TAtomic::TLiteralInt {
                        value: e1_value + e2_value,
                    },
                    oxidized::ast_defs::Bop::Minus => TAtomic::TLiteralInt {
                        value: e1_value - e2_value,
                    },
                    oxidized::ast_defs::Bop::Amp => TAtomic::TLiteralInt {
                        value: e1_value & e2_value,
                    },
                    oxidized::ast_defs::Bop::Bar => TAtomic::TLiteralInt {
                        value: e1_value | e2_value,
                    },
                    oxidized::ast_defs::Bop::Ltlt => TAtomic::TLiteralInt {
                        value: e1_value << e2_value,
                    },
                    oxidized::ast_defs::Bop::Gtgt => TAtomic::TLiteralInt {
                        value: e1_value >> e2_value,
                    },
                    oxidized::ast_defs::Bop::Percent => TAtomic::TLiteralInt {
                        value: e1_value % e2_value,
                    },
                    oxidized::ast_defs::Bop::Slash => TAtomic::TNum,
                    _ => TAtomic::TInt,
                },
                (
                    TAtomic::TInt | TAtomic::TLiteralInt { .. },
                    TAtomic::TInt | TAtomic::TLiteralInt { .. },
                ) => match operator {
                    oxidized::ast_defs::Bop::Slash => TAtomic::TNum,
                    _ => TAtomic::TInt,
                },
                _ => TAtomic::TFloat,
            });
        }
    }

    let result_type = TUnion::new(if results.len() == 1 {
        results
    } else {
        type_combiner::combine(results, None, false)
    });

    assign_arithmetic_type(
        statements_analyzer,
        tast_info,
        result_type,
        left,
        right,
        stmt_pos,
    );
}

pub(crate) fn assign_arithmetic_type(
    statements_analyzer: &StatementsAnalyzer,
    tast_info: &mut TastInfo,
    cond_type: TUnion,
    lhs_expr: &aast::Expr<(), ()>,
    rhs_expr: &aast::Expr<(), ()>,
    expr_pos: &Pos,
) {
    let mut cond_type = cond_type;
    let decision_node = DataFlowNode::get_for_variable_use(
        "composition".to_string(),
        statements_analyzer.get_hpos(expr_pos),
    );

    tast_info.data_flow_graph.add_node(decision_node.clone());

    if let Some(lhs_type) = tast_info
        .expr_types
        .get(&(lhs_expr.1.start_offset(), lhs_expr.1.end_offset()))
    {
        cond_type
            .parent_nodes
            .insert(decision_node.id.clone(), decision_node.clone());

        for (_, old_parent_node) in &lhs_type.parent_nodes {
            tast_info.data_flow_graph.add_path(
                old_parent_node,
                &decision_node,
                PathKind::Default,
                HashSet::new(),
                HashSet::new(),
            );
        }
    }

    if let Some(rhs_type) = tast_info
        .expr_types
        .get(&(rhs_expr.1.start_offset(), rhs_expr.1.end_offset()))
    {
        cond_type
            .parent_nodes
            .insert(decision_node.id.clone(), decision_node.clone());

        for (_, old_parent_node) in &rhs_type.parent_nodes {
            tast_info.data_flow_graph.add_path(
                old_parent_node,
                &decision_node,
                PathKind::Default,
                HashSet::new(),
                if cond_type.has_string() {
                    HashSet::from([TaintType::HtmlAttributeUri, TaintType::CurlUri, TaintType::RedirectUri])
                } else {
                    HashSet::new()
                },
            );
        }
    }

    tast_info.set_expr_type(&expr_pos, cond_type);
}
