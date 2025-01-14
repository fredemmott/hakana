use crate::classlike_analyzer::ClassLikeAnalyzer;
use crate::functionlike_analyzer::FunctionLikeAnalyzer;
use crate::scope_analyzer::ScopeAnalyzer;
use crate::scope_context::loop_scope::LoopScope;
use crate::scope_context::ScopeContext;
use crate::statements_analyzer::StatementsAnalyzer;
use crate::typed_ast::TastInfo;
use crate::{expression_analyzer, stmt_analyzer};
use hakana_reflection_info::analysis_result::AnalysisResult;
use hakana_reflection_info::function_context::FunctionContext;
use hakana_reflection_info::issue::{Issue, IssueKind};
use oxidized::aast;

pub(crate) fn analyze(
    scope_analyzer: &mut dyn ScopeAnalyzer,
    statements_analyzer: &StatementsAnalyzer,
    def: &aast::Def<(), ()>,
    context: &mut ScopeContext,
    loop_scope: &mut Option<LoopScope>,
    tast_info: &mut TastInfo,
    analysis_result: &mut AnalysisResult,
) {
    match def {
        aast::Def::Fun(_) => {
            let file_analyzer = scope_analyzer.get_file_analyzer();
            let mut function_analyzer = FunctionLikeAnalyzer::new(file_analyzer);
            function_analyzer.analyze_fun(def.as_fun().unwrap(), analysis_result);
        }
        aast::Def::Class(_) => {
            let file_analyzer = scope_analyzer.get_file_analyzer();
            let mut class_analyzer = ClassLikeAnalyzer::new(file_analyzer);
            class_analyzer.analyze(
                def.as_class().unwrap(),
                statements_analyzer,
                analysis_result,
            );
        }
        aast::Def::Typedef(_) | aast::Def::NamespaceUse(_) => {
            // already handled
        }
        aast::Def::Stmt(boxed) => {
            stmt_analyzer::analyze(statements_analyzer, boxed, tast_info, context, loop_scope);
        }
        aast::Def::Constant(boxed) => {
            let mut function_context = FunctionContext::new();
            function_context.calling_class = Some(
                *statements_analyzer
                    .get_file_analyzer()
                    .resolved_names
                    .get(&boxed.name.pos().start_offset())
                    .unwrap(),
            );

            let mut context = ScopeContext::new(function_context);

            expression_analyzer::analyze(
                statements_analyzer,
                &boxed.value,
                tast_info,
                &mut context,
                &mut None,
            );
        }
        aast::Def::Namespace(_) => {
            // already handled?
        }
        aast::Def::SetNamespaceEnv(_) => {
            // maybe unnecessary
        }
        aast::Def::FileAttributes(_) => {
            // not sure
        }
        aast::Def::Module(boxed) => {
            tast_info.maybe_add_issue(
                Issue::new(
                    IssueKind::UnrecognizedStatement,
                    "Unrecognized statement".to_string(),
                    statements_analyzer.get_hpos(&boxed.span),
                    &None,
                ),
                statements_analyzer.get_config(),
                statements_analyzer.get_file_path_actual(),
            );
        }
        aast::Def::SetModule(boxed) => {
            tast_info.maybe_add_issue(
                Issue::new(
                    IssueKind::UnrecognizedStatement,
                    "Unrecognized statement".to_string(),
                    statements_analyzer.get_hpos(boxed.pos()),
                    &None,
                ),
                statements_analyzer.get_config(),
                statements_analyzer.get_file_path_actual(),
            );
        }
        aast::Def::Package(_) => todo!(),
    }
}
