//! 固定 4×4 实对称正定矩阵的 Cholesky 分解与求解。
//!
//! 用于鲁棒超指向 MVDR：对 `R = Γ + λI` 分解后，分别求解 steering 的实部与虚部。

/// 对实对称正定矩阵做 Cholesky：`A = L L^T`，返回下三角 `L`。
pub fn cholesky4(a: &[[f64; 4]; 4]) -> Result<[[f64; 4]; 4], ()> {
    let mut l = [[0.0f64; 4]; 4];
    for i in 0..4 {
        for j in 0..=i {
            let mut sum = a[i][j];
            for (&li, &lj) in l[i].iter().zip(l[j].iter()).take(j) {
                sum -= li * lj;
            }
            if i == j {
                if sum <= 0.0 || !sum.is_finite() {
                    return Err(());
                }
                l[i][j] = sum.sqrt();
            } else {
                if l[j][j] == 0.0 {
                    return Err(());
                }
                l[i][j] = sum / l[j][j];
                if !l[i][j].is_finite() {
                    return Err(());
                }
            }
        }
    }
    Ok(l)
}

/// 求解 `L L^T x = b`（`L` 为下三角）。
pub fn chol_solve4(l: &[[f64; 4]; 4], b: &[f64; 4]) -> [f64; 4] {
    let mut y = [0.0f64; 4];
    for i in 0..4 {
        let mut sum = b[i];
        for k in 0..i {
            sum -= l[i][k] * y[k];
        }
        y[i] = sum / l[i][i];
    }
    let mut x = [0.0f64; 4];
    for i in (0..4).rev() {
        let mut sum = y[i];
        for k in (i + 1)..4 {
            sum -= l[k][i] * x[k];
        }
        x[i] = sum / l[i][i];
    }
    x
}

/// 用同一 Cholesky 因子分别求解实部与虚部 RHS，组合成复数解。
pub fn chol_solve4_complex(
    l: &[[f64; 4]; 4],
    re: &[f64; 4],
    im: &[f64; 4],
) -> ([f64; 4], [f64; 4]) {
    (chol_solve4(l, re), chol_solve4(l, im))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cholesky_solves_known_spd_matrix() {
        // A = [[4,1,1,1],[1,3,0,1],[1,0,2,0],[1,1,0,2]]，已知 SPD。
        let a = [
            [4.0, 1.0, 1.0, 1.0],
            [1.0, 3.0, 0.0, 1.0],
            [1.0, 0.0, 2.0, 0.0],
            [1.0, 1.0, 0.0, 2.0],
        ];
        let l = cholesky4(&a).expect("SPD 应可分解");
        // 重建 A ≈ L L^T
        for i in 0..4 {
            for j in 0..4 {
                let mut s = 0.0;
                for (&li, &lj) in l[i].iter().zip(l[j].iter()) {
                    s += li * lj;
                }
                assert!(
                    (s - a[i][j]).abs() < 1e-10,
                    "LLT[{i},{j}]={s} a={}",
                    a[i][j]
                );
            }
        }
        let b = [1.0, 2.0, 3.0, 4.0];
        let x = chol_solve4(&l, &b);
        for i in 0..4 {
            let mut ax = 0.0;
            for j in 0..4 {
                ax += a[i][j] * x[j];
            }
            assert!((ax - b[i]).abs() < 1e-9, "Ax[{i}]={ax} b={}", b[i]);
        }
        let re = [1.0, 0.0, -1.0, 0.5];
        let im = [0.0, 1.0, 0.25, -0.5];
        let (zr, zi) = chol_solve4_complex(&l, &re, &im);
        for i in 0..4 {
            let mut ar = 0.0;
            let mut ai = 0.0;
            for j in 0..4 {
                ar += a[i][j] * zr[j];
                ai += a[i][j] * zi[j];
            }
            assert!((ar - re[i]).abs() < 1e-9);
            assert!((ai - im[i]).abs() < 1e-9);
        }
    }

    #[test]
    fn cholesky_rejects_non_spd_matrix() {
        // 不定矩阵（对角含负值）。
        let a = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, -1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        assert!(cholesky4(&a).is_err());
        // 奇异半正定。
        let singular = [
            [1.0, 1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        assert!(cholesky4(&singular).is_err());
    }
}
