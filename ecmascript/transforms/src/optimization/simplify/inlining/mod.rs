use self::scope::{Scope, ScopeKind, VarType};
use crate::{pass::RepeatedJsPass, scope::IdentType};
use std::borrow::Cow;
use swc_common::{
    pass::{CompilerPass, Repeated},
    Fold, FoldWith, Visit, VisitWith,
};
use swc_ecma_ast::*;
use swc_ecma_utils::{contains_this_expr, find_ids, ident::IdentLike, undefined, Id};

mod scope;

#[derive(Debug, Default)]
pub struct Config {}

/// Note: this pass assumes that resolver is invoked before the pass.
///
/// As swc focuses on reducing gzipped file size, all strings are inlined.
///
///
/// # TODOs
///
///  - Handling of `void 0`
///  - Properly handle binary expressions.
///  - Track variables access by a function
///
/// Currently all functions are treated as a black box, and all the pass gives
/// up inlining variables across a function call or a constructor call.
pub fn inlining(_: Config) -> impl RepeatedJsPass + 'static {
    Inlining {
        phase: Phase::Analysis,
        is_first_run: true,
        changed: false,
        scope: Default::default(),
        var_decl_kind: VarDeclKind::Var,
        ident_type: IdentType::Ref,
        pat_mode: PatFoldingMode::VarDecl,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Analysis,
    Inlining,
}

impl CompilerPass for Inlining<'_> {
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("inlining")
    }
}

impl Repeated for Inlining<'_> {
    fn changed(&self) -> bool {
        self.changed
    }

    fn reset(&mut self) {
        self.changed = false;
        self.is_first_run = false;
    }
}

struct Inlining<'a> {
    phase: Phase,
    is_first_run: bool,
    changed: bool,
    scope: Scope<'a>,
    var_decl_kind: VarDeclKind,
    ident_type: IdentType,
    pat_mode: PatFoldingMode,
}

noop_fold_type!(Inlining<'_>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatFoldingMode {
    Assign,
    Param,
    CatchParam,
    VarDecl,
}

impl Inlining<'_> {
    fn fold_with_child<T>(&mut self, kind: ScopeKind, node: T) -> T
    where
        T: 'static + for<'any> FoldWith<Inlining<'any>>,
    {
        self.with_child(kind, node, |child, node| node.fold_children(child))
    }
}

impl Fold<Vec<ModuleItem>> for Inlining<'_> {
    fn fold(&mut self, mut items: Vec<ModuleItem>) -> Vec<ModuleItem> {
        let old_phase = self.phase;

        self.phase = Phase::Analysis;
        items = items.fold_children(self);

        log::debug!("Switching to Inlining phase");

        // Inline
        self.phase = Phase::Inlining;
        items = items.fold_children(self);

        self.phase = old_phase;

        items
    }
}

impl Fold<Vec<Stmt>> for Inlining<'_> {
    fn fold(&mut self, mut items: Vec<Stmt>) -> Vec<Stmt> {
        let old_phase = self.phase;

        match old_phase {
            Phase::Analysis => {
                items = items.fold_children(self);
            }
            Phase::Inlining => {
                self.phase = Phase::Analysis;
                items = items.fold_children(self);

                // Inline
                self.phase = Phase::Inlining;
                items = items.fold_children(self);

                self.phase = old_phase
            }
        }

        items
    }
}

impl Fold<VarDecl> for Inlining<'_> {
    fn fold(&mut self, decl: VarDecl) -> VarDecl {
        self.var_decl_kind = decl.kind;

        decl.fold_children(self)
    }
}

impl Fold<VarDeclarator> for Inlining<'_> {
    fn fold(&mut self, mut node: VarDeclarator) -> VarDeclarator {
        let kind = VarType::Var(self.var_decl_kind);
        node.init = node.init.fold_with(self);

        self.pat_mode = PatFoldingMode::VarDecl;

        match self.phase {
            Phase::Analysis => match node.name {
                Pat::Ident(ref name) => {
                    //
                    match &node.init {
                        None => {
                            if self.var_decl_kind != VarDeclKind::Const {
                                self.declare(name.to_id(), None, true, kind);
                            }
                        }

                        // Constants
                        Some(box e @ Expr::Lit(..)) | Some(box e @ Expr::Ident(..))
                            if self.var_decl_kind == VarDeclKind::Const =>
                        {
                            if self.is_first_run {
                                self.scope.constants.insert(name.to_id(), Some(e.clone()));
                            }
                        }
                        Some(..) if self.var_decl_kind == VarDeclKind::Const => {
                            if self.is_first_run {
                                self.scope.constants.insert(name.to_id(), None);
                            }
                        }

                        // Bindings
                        Some(box e @ Expr::Lit(..)) | Some(box e @ Expr::Ident(..)) => {
                            self.declare(name.to_id(), Some(Cow::Borrowed(&e)), false, kind);

                            if self.scope.is_inline_prevented(&e) {
                                self.scope.prevent_inline(&name.to_id());
                            }
                        }
                        Some(ref e) => {
                            if self.var_decl_kind != VarDeclKind::Const {
                                self.declare(name.to_id(), Some(Cow::Borrowed(&e)), false, kind);

                                if contains_this_expr(&node.init) {
                                    self.scope.prevent_inline(&name.to_id());
                                    return node;
                                }
                            }
                        }
                    }
                }
                _ => {}
            },
            Phase::Inlining => {
                match node.name {
                    Pat::Ident(ref name) => {
                        if self.var_decl_kind != VarDeclKind::Const {
                            let id = name.to_id();

                            log::trace!("Trying to optimize variable declaration: {:?}", id);

                            if self.scope.is_inline_prevented(&Expr::Ident(name.clone()))
                                || !self
                                    .scope
                                    .has_same_this(&id, node.init.as_ref().map(|v| &**v))
                            {
                                log::trace!("Inline is prevented for {:?}", id);
                                return node;
                            }

                            let init = node.init.take().fold_with(self);
                            log::trace!("\tInit: {:?}", init);

                            match init {
                                Some(box Expr::Ident(ref ri)) => {
                                    self.declare(
                                        name.to_id(),
                                        Some(Cow::Owned(Expr::Ident(ri.clone()))),
                                        false,
                                        kind,
                                    );
                                }

                                _ => {}
                            }

                            match init {
                                Some(ref e) => {
                                    if self.scope.is_inline_prevented(&e) {
                                        log::trace!(
                                            "Inlining is not possible as inline of the \
                                             initialization was prevented"
                                        );
                                        node.init = init;
                                        self.scope.prevent_inline(&name.to_id());
                                        return node;
                                    }
                                }
                                _ => {}
                            }

                            let e = match init {
                                None => None,
                                Some(box e @ Expr::Lit(..)) | Some(box e @ Expr::Ident(..)) => {
                                    Some(e)
                                }
                                Some(box e) => {
                                    if self.scope.is_inline_prevented(&Expr::Ident(name.clone())) {
                                        node.init = Some(box e);
                                        return node;
                                    }

                                    if let Some(cnt) = self.scope.read_cnt(&name.to_id()) {
                                        if cnt == 1 {
                                            Some(e)
                                        } else {
                                            node.init = Some(box e);
                                            return node;
                                        }
                                    } else {
                                        node.init = Some(box e);
                                        return node;
                                    }
                                }
                            };

                            // log::trace!("({}): Inserting {:?}", self.scope.depth(),
                            // name.to_id());

                            self.declare(name.to_id(), e.map(Cow::Owned), false, kind);

                            return node;
                        }
                    }
                    _ => {}
                }
            }
        }

        node.name = node.name.fold_with(self);

        node
    }
}

impl Fold<BlockStmt> for Inlining<'_> {
    fn fold(&mut self, node: BlockStmt) -> BlockStmt {
        self.fold_with_child(ScopeKind::Block, node)
    }
}

impl Fold<ArrowExpr> for Inlining<'_> {
    fn fold(&mut self, node: ArrowExpr) -> ArrowExpr {
        self.fold_with_child(ScopeKind::Fn { named: false }, node)
    }
}

impl Fold<Function> for Inlining<'_> {
    fn fold(&mut self, node: Function) -> Function {
        self.with_child(
            ScopeKind::Fn { named: false },
            node,
            move |child, mut node| {
                child.pat_mode = PatFoldingMode::Param;
                node.params = node.params.fold_with(child);
                node.body = match node.body {
                    None => None,
                    Some(v) => Some(v.fold_children(child)),
                };

                node
            },
        )
    }
}

impl Fold<FnDecl> for Inlining<'_> {
    fn fold(&mut self, node: FnDecl) -> FnDecl {
        if self.phase == Phase::Analysis {
            self.declare(
                node.ident.to_id(),
                None,
                true,
                VarType::Var(VarDeclKind::Var),
            );
        }

        let function = node.function;

        let function = self.with_child(
            ScopeKind::Fn { named: true },
            function,
            |child, mut node| {
                child.pat_mode = PatFoldingMode::Param;
                node.params = node.params.fold_with(child);
                node.body = match node.body {
                    None => None,
                    Some(v) => Some(v.fold_children(child)),
                };

                node
            },
        );
        FnDecl { function, ..node }
    }
}

impl Fold<FnExpr> for Inlining<'_> {
    fn fold(&mut self, node: FnExpr) -> FnExpr {
        if let Some(ref ident) = node.ident {
            self.scope.add_write(&ident.to_id(), true);
        }

        FnExpr {
            function: node.function.fold_with(self),
            ..node
        }
    }
}

impl Fold<IfStmt> for Inlining<'_> {
    fn fold(&mut self, mut node: IfStmt) -> IfStmt {
        node.test = node.test.fold_with(self);

        node.cons = self.fold_with_child(ScopeKind::Cond, node.cons);
        node.alt = self.fold_with_child(ScopeKind::Cond, node.alt);

        node
    }
}

impl Fold<SwitchCase> for Inlining<'_> {
    fn fold(&mut self, node: SwitchCase) -> SwitchCase {
        self.fold_with_child(ScopeKind::Block, node)
    }
}

impl Fold<CatchClause> for Inlining<'_> {
    fn fold(&mut self, node: CatchClause) -> CatchClause {
        self.with_child(ScopeKind::Block, node, move |child, mut node| {
            child.pat_mode = PatFoldingMode::CatchParam;
            node.param = node.param.fold_with(child);
            match child.phase {
                Phase::Analysis => {
                    let ids: Vec<Id> = find_ids(&node.param);
                    for id in ids {
                        child.scope.prevent_inline(&id);
                    }
                }
                Phase::Inlining => {}
            }

            node.body = node.body.fold_with(child);

            node
        })
    }
}

impl Fold<CallExpr> for Inlining<'_> {
    fn fold(&mut self, mut node: CallExpr) -> CallExpr {
        node.callee = node.callee.fold_with(self);

        if self.phase == Phase::Analysis {
            match node.callee {
                ExprOrSuper::Expr(ref callee) => {
                    self.scope.mark_this_sensitive(&callee);
                }

                _ => {}
            }
        }

        node.args = node.args.fold_with(self);

        self.scope.store_inline_barrier(self.phase);

        node
    }
}

impl Fold<NewExpr> for Inlining<'_> {
    fn fold(&mut self, mut node: NewExpr) -> NewExpr {
        node.callee = node.callee.fold_with(self);
        if self.phase == Phase::Analysis {
            self.scope.mark_this_sensitive(&node.callee);
        }

        node.args = node.args.fold_with(self);

        self.scope.store_inline_barrier(self.phase);

        node
    }
}

impl Fold<AssignExpr> for Inlining<'_> {
    fn fold(&mut self, e: AssignExpr) -> AssignExpr {
        log::trace!("{:?}; Fold<AssignExpr>", self.phase);
        self.pat_mode = PatFoldingMode::Assign;
        let e = AssignExpr {
            left: match e.left {
                PatOrExpr::Expr(left) | PatOrExpr::Pat(box Pat::Expr(left)) => {
                    //
                    match *left {
                        Expr::Member(ref left) => {
                            log::trace!("Assign to member expression!");
                            let mut v = IdentListVisitor {
                                scope: &mut self.scope,
                            };

                            left.visit_with(&mut v);
                            e.right.visit_with(&mut v);
                        }

                        _ => {}
                    }

                    PatOrExpr::Expr(left)
                }
                PatOrExpr::Pat(p) => PatOrExpr::Pat(p.fold_with(self)),
            },
            right: e.right.fold_with(self),
            ..e
        };

        match e.op {
            op!("=") => {}
            _ => {
                let mut v = IdentListVisitor {
                    scope: &mut self.scope,
                };

                e.left.visit_with(&mut v);
                e.right.visit_with(&mut v)
            }
        }

        if self.scope.is_inline_prevented(&e.right) {
            // Prevent inline for lhd
            let ids: Vec<Id> = find_ids(&e.left);
            for id in ids {
                self.scope.prevent_inline(&id);
            }
            return e;
        }

        match *e.right {
            Expr::Lit(..) | Expr::Ident(..) => {
                //
                match e.left {
                    PatOrExpr::Pat(box Pat::Ident(ref i))
                    | PatOrExpr::Expr(box Expr::Ident(ref i)) => {
                        let id = i.to_id();
                        self.scope.add_write(&id, false);

                        if let Some(var) = self.scope.find_binding(&id) {
                            if !var.is_inline_prevented() {
                                *var.value.borrow_mut() = Some(*e.right.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }

            _ => {}
        }

        e
    }
}

impl Fold<MemberExpr> for Inlining<'_> {
    fn fold(&mut self, mut e: MemberExpr) -> MemberExpr {
        e.obj = e.obj.fold_with(self);
        if e.computed {
            e.prop = e.prop.fold_with(self);
        }

        e
    }
}

impl Fold<Expr> for Inlining<'_> {
    fn fold(&mut self, node: Expr) -> Expr {
        let node: Expr = node.fold_children(self);

        // Codes like
        //
        //      var y;
        //      y = x;
        //      use(y)
        //
        //  should be transformed to
        //
        //      var y;
        //      x;
        //      use(x)
        //
        // We cannot know if this is possible while analysis phase
        if self.phase == Phase::Inlining {
            match node {
                Expr::Assign(e @ AssignExpr { op: op!("="), .. }) => {
                    match e.left {
                        PatOrExpr::Pat(box Pat::Ident(ref i))
                        | PatOrExpr::Expr(box Expr::Ident(ref i)) => {
                            if let Some(var) = self.scope.find_binding_from_current(&i.to_id()) {
                                if var.is_undefined.get() && !var.is_inline_prevented() {
                                    if !self.scope.is_inline_prevented(&e.right) {
                                        *var.value.borrow_mut() = Some(*e.right.clone());
                                        var.is_undefined.set(false);
                                        return *e.right;
                                    }
                                }
                            }
                        }
                        _ => {}
                    }

                    return Expr::Assign(e);
                }

                _ => {}
            }
        }

        match node {
            Expr::Ident(ref i) => {
                let id = i.to_id();
                if self.is_first_run {
                    if let Some(expr) = self.scope.find_constant(&id) {
                        self.changed = true;
                        return expr.clone().fold_with(self);
                    }
                }

                match self.phase {
                    Phase::Analysis => {
                        self.scope.add_read(&id);
                    }
                    Phase::Inlining => {
                        log::trace!("Trying to inline: {:?}", id);
                        let expr = if let Some(var) = self.scope.find_binding(&id) {
                            log::trace!("VarInfo: {:?}", var);
                            if !var.is_inline_prevented() {
                                let expr = var.value.borrow();

                                if let Some(expr) = &*expr {
                                    if node != *expr {
                                        self.changed = true;
                                    }

                                    Some(expr.clone())
                                } else {
                                    if var.is_undefined.get() {
                                        return *undefined(i.span);
                                    } else {
                                        log::trace!("Not a cheap expression");
                                        None
                                    }
                                }
                            } else {
                                log::trace!("Inlining is prevented");
                                None
                            }
                        } else {
                            None
                        };

                        if let Some(expr) = expr {
                            return expr;
                        }
                    }
                }
            }

            _ => {}
        }

        node
    }
}

impl Fold<UpdateExpr> for Inlining<'_> {
    fn fold(&mut self, node: UpdateExpr) -> UpdateExpr {
        let mut v = IdentListVisitor {
            scope: &mut self.scope,
        };

        node.arg.visit_with(&mut v);
        node
    }
}

impl Fold<UnaryExpr> for Inlining<'_> {
    fn fold(&mut self, node: UnaryExpr) -> UnaryExpr {
        match node.op {
            op!("delete") => {
                let mut v = IdentListVisitor {
                    scope: &mut self.scope,
                };

                node.arg.visit_with(&mut v);
                return node;
            }

            _ => {}
        }

        node.fold_children(self)
    }
}

impl Fold<Pat> for Inlining<'_> {
    fn fold(&mut self, node: Pat) -> Pat {
        let node: Pat = node.fold_children(self);

        match node {
            Pat::Ident(ref i) => match self.pat_mode {
                PatFoldingMode::Param => {
                    self.declare(
                        i.to_id(),
                        Some(Cow::Owned(Expr::Ident(i.clone()))),
                        false,
                        VarType::Param,
                    );
                }
                PatFoldingMode::CatchParam => {
                    self.declare(
                        i.to_id(),
                        Some(Cow::Owned(Expr::Ident(i.clone()))),
                        false,
                        VarType::Var(VarDeclKind::Var),
                    );
                }
                PatFoldingMode::VarDecl => {}
                PatFoldingMode::Assign => {
                    if let Some(..) = self.scope.find_binding_from_current(&i.to_id()) {
                    } else {
                        self.scope.add_write(&i.to_id(), false);
                    }
                }
            },

            _ => {}
        }

        node
    }
}

impl Fold<ForInStmt> for Inlining<'_> {
    fn fold(&mut self, mut node: ForInStmt) -> ForInStmt {
        self.pat_mode = PatFoldingMode::Param;
        node.left = node.left.fold_with(self);

        {
            node.left.visit_with(&mut IdentListVisitor {
                scope: &mut self.scope,
            });
        }

        {
            node.right.visit_with(&mut IdentListVisitor {
                scope: &mut self.scope,
            });
        }

        node.right = node.right.fold_with(self);
        node.body = self.fold_with_child(ScopeKind::Loop, node.body);

        node
    }
}

impl Fold<ForOfStmt> for Inlining<'_> {
    fn fold(&mut self, mut node: ForOfStmt) -> ForOfStmt {
        self.pat_mode = PatFoldingMode::Param;
        node.left = node.left.fold_with(self);

        {
            node.left.visit_with(&mut IdentListVisitor {
                scope: &mut self.scope,
            });
        }
        {
            node.right.visit_with(&mut IdentListVisitor {
                scope: &mut self.scope,
            });
        }

        node.right = node.right.fold_with(self);
        node.body = self.fold_with_child(ScopeKind::Loop, node.body);

        node
    }
}

impl Fold<ForStmt> for Inlining<'_> {
    fn fold(&mut self, mut node: ForStmt) -> ForStmt {
        node.init = node.init.fold_with(self);

        {
            node.init.visit_with(&mut IdentListVisitor {
                scope: &mut self.scope,
            });
        }
        {
            node.test.visit_with(&mut IdentListVisitor {
                scope: &mut self.scope,
            });
        }
        {
            node.update.visit_with(&mut IdentListVisitor {
                scope: &mut self.scope,
            });
        }

        node.test = node.test.fold_with(self);
        node.update = node.update.fold_with(self);
        node.body = self.fold_with_child(ScopeKind::Loop, node.body);

        if node.init.is_none() && node.test.is_none() && node.update.is_none() {
            self.scope.store_inline_barrier(self.phase);
        }

        node
    }
}

impl Fold<WhileStmt> for Inlining<'_> {
    fn fold(&mut self, mut node: WhileStmt) -> WhileStmt {
        {
            node.test.visit_with(&mut IdentListVisitor {
                scope: &mut self.scope,
            });
        }

        node.test = node.test.fold_with(self);
        node.body = self.fold_with_child(ScopeKind::Loop, node.body);

        node
    }
}

impl Fold<DoWhileStmt> for Inlining<'_> {
    fn fold(&mut self, mut node: DoWhileStmt) -> DoWhileStmt {
        {
            node.test.visit_with(&mut IdentListVisitor {
                scope: &mut self.scope,
            });
        }

        node.test = node.test.fold_with(self);
        node.body = self.fold_with_child(ScopeKind::Loop, node.body);

        node
    }
}

impl Fold<BinExpr> for Inlining<'_> {
    fn fold(&mut self, node: BinExpr) -> BinExpr {
        match node.op {
            op!("&&") | op!("||") => BinExpr {
                left: node.left.fold_with(self),
                ..node
            },
            _ => node.fold_children(self),
        }
    }
}

impl Fold<TryStmt> for Inlining<'_> {
    fn fold(&mut self, node: TryStmt) -> TryStmt {
        node.block.visit_with(&mut IdentListVisitor {
            scope: &mut self.scope,
        });

        TryStmt {
            // TODO:
            //            block: node.block.fold_with(self),
            handler: node.handler.fold_with(self),
            ..node
        }
    }
}

#[derive(Debug)]
struct IdentListVisitor<'a, 'b> {
    scope: &'a mut Scope<'b>,
}

impl Visit<MemberExpr> for IdentListVisitor<'_, '_> {
    fn visit(&mut self, node: &MemberExpr) {
        node.obj.visit_with(self);

        if node.computed {
            node.prop.visit_with(self);
        }
    }
}

impl Visit<Ident> for IdentListVisitor<'_, '_> {
    fn visit(&mut self, node: &Ident) {
        self.scope.add_write(&node.to_id(), true);
    }
}
