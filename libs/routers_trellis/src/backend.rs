use crate::trellis::INF_W;

#[derive(Clone, Copy)]
pub enum Backend {
    Scalar,
    #[cfg(target_arch = "x86_64")]
    Avx2,
    #[cfg(all(target_arch = "x86_64", feature = "avx512"))]
    Avx512,
}

impl Default for Backend {
    fn default() -> Self {
        #[cfg(all(target_arch = "x86_64", feature = "avx512"))]
        {
            if std::is_x86_feature_detected!("avx512f") {
                return Backend::Avx512;
            }
            if std::is_x86_feature_detected!("avx2") {
                return Backend::Avx2;
            }
        }

        #[cfg(all(target_arch = "x86_64", not(feature = "avx512")))]
        {
            if std::is_x86_feature_detected!("avx2") {
                return Backend::Avx2;
            }
        }

        Backend::Scalar
    }
}

impl Backend {
    pub fn dispatch(&self, cur: &[u32], w: &[u32], from: usize, nw: usize, next: &mut [u32]) {
        match self {
            Backend::Scalar => step_scalar(cur, w, from, nw, next),
            #[cfg(target_arch = "x86_64")]
            Backend::Avx2 => unsafe { step_avx2(cur, w, from, nw, next) },
            #[cfg(all(target_arch = "x86_64", feature = "avx512"))]
            Backend::Avx512 => unsafe { step_avx512(cur, w, from, nw, next) },
        }
    }
}

#[inline]
fn step_scalar(cur: &[u32], w: &[u32], from: usize, nw: usize, next: &mut [u32]) {
    for x in next[..nw].iter_mut() {
        *x = INF_W;
    }

    for i in 0..from {
        let ci = cur[i];
        if ci == INF_W {
            continue;
        }
        let row = &w[i * nw..i * nw + nw];
        for j in 0..nw {
            let v = ci + row[j];
            if v < next[j] {
                next[j] = v;
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn step_avx2(cur: &[u32], w: &[u32], from: usize, nw: usize, next: &mut [u32]) {
    use std::arch::x86_64::*;
    for x in next[..nw].iter_mut() {
        *x = INF_W;
    }
    let np = next.as_mut_ptr();
    for i in 0..from {
        let ci = cur[i];
        if ci == INF_W {
            continue;
        }
        let vci = _mm256_set1_epi32(ci as i32);
        let base = i * nw;
        let mut j = 0;
        while j + 8 <= nw {
            let vw = _mm256_loadu_si256(w.as_ptr().add(base + j) as *const __m256i);
            // Sums stay < 2^31, so unsigned min over epi32 lanes is correct.
            let vs = _mm256_add_epi32(vci, vw);
            let vn = _mm256_loadu_si256(np.add(j) as *const __m256i);
            _mm256_storeu_si256(np.add(j) as *mut __m256i, _mm256_min_epu32(vn, vs));
            j += 8;
        }
        while j < nw {
            let v = ci + w[base + j];
            if v < *np.add(j) {
                *np.add(j) = v;
            }
            j += 1;
        }
    }
}

#[cfg(all(target_arch = "x86_64", feature = "avx512"))]
#[target_feature(enable = "avx512f")]
unsafe fn step_avx512(cur: &[u32], w: &[u32], from: usize, nw: usize, next: &mut [u32]) {
    use std::arch::x86_64::*;
    for x in next[..nw].iter_mut() {
        *x = INF_W;
    }
    let np = next.as_mut_ptr();
    for i in 0..from {
        let ci = cur[i];
        if ci == INF_W {
            continue;
        }
        let vci = _mm512_set1_epi32(ci as i32);
        let base = i * nw;
        let mut j = 0;
        while j + 16 <= nw {
            let vw = _mm512_loadu_si512(w.as_ptr().add(base + j) as *const i32);
            let vs = _mm512_add_epi32(vci, vw);
            let vn = _mm512_loadu_si512(np.add(j) as *const i32);
            _mm512_storeu_si512(np.add(j) as *mut i32, _mm512_min_epu32(vn, vs));
            j += 16;
        }
        while j < nw {
            let v = ci + w[base + j];
            if v < *np.add(j) {
                *np.add(j) = v;
            }
            j += 1;
        }
    }
}
