//! Braille plot of a frequency response curve (U+2800 + dot bitmask, 2x4 subpixels/cell).
//! Shared by the TUI widget and the CLI's stdout curve.

const F_LO: f64 = 20.0;
const F_HI: f64 = 20_000.0;
const MIN_SPAN_DB: f64 = 24.0; // clamp to at least a +/-12dB window

/// `n` frequencies (Hz), log-spaced from 20Hz to 20kHz.
pub fn log_freqs(n: usize) -> Vec<f64> {
    if n < 2 {
        return vec![F_LO];
    }
    let log_lo = F_LO.log10();
    let log_hi = F_HI.log10();
    (0..n)
        .map(|i| {
            let t = i as f64 / (n - 1) as f64;
            10f64.powf(log_lo + t * (log_hi - log_lo))
        })
        .collect()
}

/// Render `db_at(freq)` as a braille plot `height` rows tall, `width` columns wide.
pub fn plot(db_at: impl Fn(f64) -> f64, width: usize, height: usize) -> Vec<String> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let sub_cols = width * 2;
    let sub_rows = height * 4;

    let freqs = log_freqs(sub_cols);
    let values: Vec<f64> = freqs.iter().map(|&f| db_at(f)).collect();

    let curve_min = values.iter().cloned().fold(f64::INFINITY, f64::min);
    let curve_max = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let lo = curve_min.min(-MIN_SPAN_DB / 2.0);
    let hi = curve_max.max(MIN_SPAN_DB / 2.0);
    let span = (hi - lo).max(1e-6);

    let row_for = |db: f64| -> usize {
        let t = (hi - db) / span; // 0 at top (hi dB), 1 at bottom (lo dB)
        ((t * (sub_rows - 1) as f64).round() as isize).clamp(0, sub_rows as isize - 1) as usize
    };

    let mut dots = vec![vec![false; sub_cols]; sub_rows];
    let mut prev_row = row_for(values[0]);
    for (col, &db) in values.iter().enumerate() {
        let row = row_for(db);
        let (from, to) = if row <= prev_row {
            (row, prev_row)
        } else {
            (prev_row, row)
        };
        for row in dots.iter_mut().take(to + 1).skip(from) {
            row[col] = true;
        }
        prev_row = row;
    }

    let mut out = Vec::with_capacity(height);
    for cell_row in 0..height {
        let mut line = String::with_capacity(width);
        for cell_col in 0..width {
            let r0 = cell_row * 4;
            let c0 = cell_col * 2;
            let mut mask: u8 = 0;
            let bits = [
                (0, 0, 0x01u8),
                (1, 0, 0x02),
                (2, 0, 0x04),
                (0, 1, 0x08),
                (1, 1, 0x10),
                (2, 1, 0x20),
                (3, 0, 0x40),
                (3, 1, 0x80),
            ];
            for (dr, dc, bit) in bits {
                if dots[r0 + dr][c0 + dc] {
                    mask |= bit;
                }
            }
            let cp = 0x2800u32 + mask as u32;
            line.push(char::from_u32(cp).unwrap_or(' '));
        }
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_zero_response_is_a_stable_single_line() {
        let lines = plot(|_f| 0.0, 20, 4);
        assert_eq!(lines.len(), 4);
        for line in &lines {
            assert_eq!(line.chars().count(), 20);
        }
        // a flat curve should light exactly one row of cells (the dot line doesn't
        // spread vertically since every column maps to the same subpixel row)
        let lit_rows = lines
            .iter()
            .filter(|l| l.chars().any(|c| c != '\u{2800}'))
            .count();
        assert_eq!(
            lit_rows,
            1,
            "flat curve should occupy exactly one cell row:\n{}",
            lines.join("\n")
        );
        // rendering the same input twice must be byte-identical (deterministic/reproducible)
        assert_eq!(lines, plot(|_f| 0.0, 20, 4));
    }

    #[test]
    fn log_freqs_spans_20_to_20k() {
        let f = log_freqs(200);
        assert_eq!(f.len(), 200);
        assert!((f[0] - 20.0).abs() < 0.01);
        assert!((f[199] - 20_000.0).abs() < 1.0);
    }
}
