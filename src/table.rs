//! Text-table rendering shared by the CLI, `report` and `render`.
//!
//! Column widths are computed from the widest cell so output stays readable
//! without a terminal-size guess, and everything is plain ASCII so the output can
//! be pasted into a runbook or a ticket.

/// A simple left-aligned table.
#[derive(Debug, Clone)]
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    aligns_right: Vec<bool>,
}

impl Table {
    /// Create a table with the given headers (all left-aligned).
    pub fn new<I, S>(headers: I) -> Table
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let headers: Vec<String> = headers.into_iter().map(Into::into).collect();
        Table {
            aligns_right: vec![false; headers.len()],
            rows: Vec::new(),
            headers,
        }
    }

    /// Mark a column as right-aligned (numbers read better that way).
    pub fn align_right(&mut self, column: usize) {
        if let Some(flag) = self.aligns_right.get_mut(column) {
            *flag = true;
        }
    }

    /// Append a row. Rows longer than the header are truncated, shorter ones are
    /// padded, so a malformed row can never corrupt the layout.
    pub fn push<I, S>(&mut self, cells: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut row: Vec<String> = cells.into_iter().map(Into::into).collect();
        row.truncate(self.headers.len());
        while row.len() < self.headers.len() {
            row.push(String::new());
        }
        self.rows.push(row);
    }

    /// Number of data rows.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// True when there are no data rows.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn widths(&self) -> Vec<usize> {
        let mut widths: Vec<usize> = self.headers.iter().map(|header| header.len()).collect();
        for row in &self.rows {
            for (position, cell) in row.iter().enumerate() {
                if position < widths.len() {
                    widths[position] = widths[position].max(cell.chars().count());
                }
            }
        }
        widths
    }

    /// Render the table with headers and a separator line.
    pub fn render(&self) -> String {
        let widths = self.widths();
        let mut out = String::new();
        out.push_str(&self.render_row(&self.headers, &widths));
        let separator: Vec<String> = widths.iter().map(|width| "-".repeat(*width)).collect();
        out.push_str(&self.render_row(&separator, &widths));
        for row in &self.rows {
            out.push_str(&self.render_row(row, &widths));
        }
        out
    }

    /// Render the table without headers (used for continuation blocks).
    pub fn render_body(&self) -> String {
        let widths = self.widths();
        self.rows
            .iter()
            .map(|row| self.render_row(row, &widths))
            .collect()
    }

    fn render_row(&self, cells: &[String], widths: &[usize]) -> String {
        let mut line = String::new();
        for (position, cell) in cells.iter().enumerate() {
            let width = widths.get(position).copied().unwrap_or(0);
            if position > 0 {
                line.push_str("  ");
            }
            let padded = if self.aligns_right.get(position).copied().unwrap_or(false) {
                format!("{cell:>width$}")
            } else {
                format!("{cell:<width$}")
            };
            line.push_str(&padded);
        }
        line.trim_end().to_string() + "\n"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_aligned_columns() {
        let mut table = Table::new(["irq", "name", "affinity"]);
        table.push(["24", "nvme0q0", "0-7"]);
        table.push(["122", "eth0-rx-0", "0-1"]);
        let rendered = table.render();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 4, "header, separator, two rows");
        assert!(lines[0].starts_with("irq  name"));
        assert!(lines[1].starts_with("---"));
        // Columns start at fixed offsets because every cell is padded to the width
        // of its column (name is 9 wide here: "eth0-rx-0").
        assert_eq!(lines[2], "24   nvme0q0    0-7");
        assert_eq!(lines[3], "122  eth0-rx-0  0-1");
        // Trailing padding is stripped so the output stays paste-friendly.
        assert!(lines.iter().all(|line| line.trim_end() == *line));
    }

    #[test]
    fn right_alignment_pads_on_the_left() {
        let mut table = Table::new(["cpu", "hits"]);
        table.align_right(1);
        table.push(["0", "12"]);
        table.push(["1", "3"]);
        let rendered = table.render();
        assert!(rendered.contains("     12"));
        assert!(rendered.contains("      3"));
    }

    #[test]
    fn ragged_rows_are_normalised() {
        let mut table = Table::new(["a", "b"]);
        table.push(["only-one"]);
        table.push(["x", "y", "extra"]);
        assert_eq!(table.len(), 2);
        let rendered = table.render();
        assert!(rendered.contains("only-one"));
        assert!(!rendered.contains("extra"), "overflow cells are dropped");
    }

    #[test]
    fn empty_table_still_renders_headers() {
        let table = Table::new(["a", "b"]);
        assert!(table.is_empty());
        let rendered = table.render();
        assert_eq!(rendered.lines().count(), 2);
    }

    #[test]
    fn body_rendering_omits_headers() {
        let mut table = Table::new(["a"]);
        table.push(["1"]);
        assert_eq!(table.render_body(), "1\n");
    }
}
