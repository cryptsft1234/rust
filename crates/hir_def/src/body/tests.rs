mod block;

use base_db::{fixture::WithFixture, SourceDatabase};
use expect_test::Expect;

use crate::{test_db::TestDB, ModuleDefId};

use super::*;

fn lower(ra_fixture: &str) -> Arc<Body> {
    let db = crate::test_db::TestDB::with_files(ra_fixture);

    let krate = db.crate_graph().iter().next().unwrap();
    let def_map = db.crate_def_map(krate);
    let mut fn_def = None;
    'outer: for (_, module) in def_map.modules() {
        for decl in module.scope.declarations() {
            match decl {
                ModuleDefId::FunctionId(it) => {
                    fn_def = Some(it);
                    break 'outer;
                }
                _ => {}
            }
        }
    }

    db.body(fn_def.unwrap().into())
}

fn check_diagnostics(ra_fixture: &str) {
    let db: TestDB = TestDB::with_files(ra_fixture);
    db.check_diagnostics();
}

fn block_def_map_at(ra_fixture: &str) -> String {
    let (db, position) = crate::test_db::TestDB::with_position(ra_fixture);

    let module = db.module_at_position(position);
    module.def_map(&db).dump(&db)
}

fn check_block_scopes_at(ra_fixture: &str, expect: Expect) {
    let (db, position) = crate::test_db::TestDB::with_position(ra_fixture);

    let module = db.module_at_position(position);
    let actual = module.def_map(&db).dump_block_scopes(&db);
    expect.assert_eq(&actual);
}

fn check_at(ra_fixture: &str, expect: Expect) {
    let actual = block_def_map_at(ra_fixture);
    expect.assert_eq(&actual);
}

#[test]
fn your_stack_belongs_to_me() {
    cov_mark::check!(your_stack_belongs_to_me);
    lower(
        "
macro_rules! n_nuple {
    ($e:tt) => ();
    ($($rest:tt)*) => {{
        (n_nuple!($($rest)*)None,)
    }};
}
fn main() { n_nuple!(1,2,3); }
",
    );
}

#[test]
fn macro_resolve() {
    // Regression test for a path resolution bug introduced with inner item handling.
    lower(
        r"
macro_rules! vec {
    () => { () };
    ($elem:expr; $n:expr) => { () };
    ($($x:expr),+ $(,)?) => { () };
}
mod m {
    fn outer() {
        let _ = vec![FileSet::default(); self.len()];
    }
}
      ",
    );
}

#[test]
fn unresolved_macro_diag() {
    check_diagnostics(
        r#"
fn f() {
    m!();
  //^^^^ UnresolvedMacroCall
}
      "#,
    );
}

