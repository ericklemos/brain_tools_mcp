//! A guarda do shell precisa apontar para o substituto, não só negar.
//! Sem isso, `mv`/`cp` viram tentativa e erro em vez de `brain_move`.

use brain_exec::guard;

#[test]
fn mv_and_cp_are_redirected_to_brain_move() {
    let git_mv = guard::check("git mv a.rs b.rs").expect("git mv deve ser negado");
    assert!(
        guard::message(&git_mv).contains("brain_move"),
        "{}",
        guard::message(&git_mv)
    );

    for command in ["mv a.rs b.rs", "cp a.rs b.rs"] {
        let denial =
            guard::check(command).unwrap_or_else(|| panic!("{command} deveria ser negado"));
        let message = guard::message(&denial);
        assert!(message.contains("brain_move"), "{command}: {message}");
    }
}
