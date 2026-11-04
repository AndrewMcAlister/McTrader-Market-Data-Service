//! The two structural pieces every set-based statement needs.
//!
//! SQL Server caps a statement at 2,100 parameters, and the daily load wants to write tens of
//! thousands of rows in one short transaction. The source service achieved that with
//! `executemany`, which reuses one 11- or 12-parameter statement per row. TDS has no
//! equivalent, so the Rust store writes `INSERT ... VALUES (...), (...)` and
//! `MERGE ... USING (VALUES ...)` with as many rows as the parameter ceiling allows.

use super::query::MAX_PARAMS_PER_STATEMENT;

/// `@P1,@P2,...` beginning at `first`, for an `IN (...)` list.
pub fn placeholders(first: usize, count: usize) -> String {
    (0..count)
        .map(|offset| format!("@P{}", first + offset))
        .collect::<Vec<_>>()
        .join(",")
}

/// `(@P1,...,@PN),(@PN+1,...)` for a multi-row `VALUES` clause starting at `@P1`.
pub fn values_rows(rows: usize, params_per_row: usize) -> String {
    if rows == 0 || params_per_row == 0 {
        return String::new();
    }
    let mut out = String::with_capacity(rows * params_per_row * 6);
    for row in 0..rows {
        if row > 0 {
            out.push(',');
        }
        out.push('(');
        out.push_str(&placeholders(row * params_per_row + 1, params_per_row));
        out.push(')');
    }
    out
}

/// How many rows fit in one statement without crossing [`MAX_PARAMS_PER_STATEMENT`].
pub fn rows_per_statement(params_per_row: usize) -> usize {
    if params_per_row == 0 {
        return 0;
    }
    (MAX_PARAMS_PER_STATEMENT / params_per_row).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_numbered_from_the_requested_first() {
        assert_eq!(placeholders(1, 3), "@P1,@P2,@P3");
        assert_eq!(placeholders(4, 2), "@P4,@P5");
        assert_eq!(placeholders(9, 0), "");
    }

    #[test]
    fn a_values_clause_numbers_every_row_continuously() {
        assert_eq!(values_rows(2, 3), "(@P1,@P2,@P3),(@P4,@P5,@P6)");
        assert_eq!(values_rows(1, 2), "(@P1,@P2)");
        assert_eq!(values_rows(0, 3), "");
    }

    #[test]
    fn the_row_count_respects_the_parameter_ceiling() {
        let rows = rows_per_statement(11);
        assert_eq!(rows, 181);
        assert!(rows * 11 <= MAX_PARAMS_PER_STATEMENT);
        assert!((rows + 1) * 11 > MAX_PARAMS_PER_STATEMENT);
        assert_eq!(rows_per_statement(12), 166);
        assert_eq!(rows_per_statement(0), 0);
    }

    #[test]
    fn a_values_clause_never_exceeds_the_parameter_ceiling() {
        for params_per_row in [1usize, 2, 11, 12, 1500, 2000] {
            let rows = rows_per_statement(params_per_row);
            let clause = values_rows(rows, params_per_row);
            let count = clause.matches("@P").count();
            assert!(
                count <= MAX_PARAMS_PER_STATEMENT,
                "{params_per_row} -> {count}"
            );
        }
    }
}
