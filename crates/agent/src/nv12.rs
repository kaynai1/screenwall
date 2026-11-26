//! BGRA → NV12 转换（H.264 编码器的输入格式）。
//!
//! 采集链路给的是紧凑 BGRA；硬件 H.264 编码器只吃 NV12（Y 平面 + 交错的 UV）。
//! 4K 每帧 8.3M 像素，纯标量转换要几十毫秒，会把 60fps 直接吃掉，所以：
//!
//! - 亮度与色度都走 AVX2 的 32 位整数通道（一次 8 像素），没有 AVX2 时回退标量；
//! - 色度按 2×2 取平均，两行一起算，避免先算出每像素 U/V 再二次遍历；
//! - **按两行分块**：同一对行连着做完两行亮度再算这组色度，两行（4K 下
//!   各 15 KB）还在 L1/L2 里就被色度复用，源数据每帧只从内存读一遍。
//!   分开两趟在 4K 下要读 66 MB，分块后只读 33 MB；
//! - 宽高为奇数时补一列/一行（H.264 的 4:2:0 要求偶数尺寸），补出来的像素
//!   复制最后一列/行。
//!
//! 试过把亮度和色度改成 16 位 `maddubs`（含 `phaddw`）实现，在本机
//! i5-12400F 上反而慢 20–30%：横向相加与打包都挤在 port 5，抵消了省下的
//! 32 位乘法，所以保留 32 位版本。AVX-VNNI 的 `vpdpbusd` 理论上一条指令能算完
//! 一个像素，但这颗 CPU 上执行会报非法指令（CPUID 报告支持，实际不可用），
//! 也不能依赖。
//!
//! 色彩空间固定为 **BT.709 limited range**（HD 视频的标准矩阵，浏览器解码
//! H.264 时默认也按它解释）。系数是 libyuv 用的那组定点近似：
//!
//! ```text
//! Y = ( 47R + 157G +  16B + 128) >> 8 + 16
//! U = (-26R -  87G + 112B + 128) >> 8 + 128
//! V = (112R - 102G -  10B + 128) >> 8 + 128
//! ```

/// NV12 的偶数尺寸：H.264 的 4:2:0 色度按 2×2 采样，宽高必须是偶数。
pub fn padded_dims(width: usize, height: usize) -> (usize, usize) {
    (width + (width & 1), height + (height & 1))
}

/// NV12 缓冲区大小（Y 平面 + 交错的 UV 平面）。
pub fn nv12_len(width: usize, height: usize) -> usize {
    let (w, h) = padded_dims(width, height);
    w * h * 3 / 2
}

/// 把紧凑 BGRA 转成 NV12，`dst` 会按 [`nv12_len`] 扩容。
///
/// 返回实际写入的尺寸（偶数）。按两行一块处理：这两行的亮度和这组色度
/// 紧接着算完，源数据每帧只从内存里读一遍（而不是亮度一趟、色度又一趟）。
pub fn bgra_to_nv12(src: &[u8], width: usize, height: usize, dst: &mut Vec<u8>) -> (usize, usize) {
    let (w2, h2) = padded_dims(width, height);
    if w2 == 0 || h2 == 0 {
        dst.clear();
        return (0, 0);
    }
    // 每个字节都会被覆盖，所以只在长度不对时才扩容；复用同一块缓冲区时
    // 省掉每帧一次全尺寸清零（4K 是 12.4 MB，约 0.5 ms）。
    let total = w2 * h2 * 3 / 2;
    if dst.len() != total {
        dst.clear();
        dst.resize(total, 0);
    }

    // Y 平面在前，UV 平面在后；两个平面互不重叠，可以同时可变借用。
    let uv_start = w2 * h2;
    let (y_plane, uv_plane) = dst.split_at_mut(uv_start);
    let last_src_row = height.saturating_sub(1);

    for cy in 0..h2 / 2 {
        let src0 = (cy * 2).min(last_src_row);
        let src1 = (cy * 2 + 1).min(last_src_row);
        let row0 = &src[src0 * width * 4..];
        let row1 = &src[src1 * width * 4..];
        let (y0, rest) = y_plane[cy * 2 * w2..].split_at_mut(w2);
        let (y1, _) = rest.split_at_mut(w2);
        let uv = &mut uv_plane[cy * w2..(cy + 1) * w2];
        // 这两行刚写过，紧接着的色度直接从缓存里再读一遍，不回内存。
        luma_row(row0, width, y0);
        luma_row(row1, width, y1);
        chroma_row(row0, row1, width, uv);
    }

    (w2, h2)
}

/// 一行亮度。AVX2 一次 8 像素，尾部（含奇数宽度的补列）走标量。
#[cfg(target_arch = "x86_64")]
fn luma_row(row: &[u8], width: usize, out: &mut [u8]) {
    let mut x = 0usize;
    if width >= 8 && is_avx2() {
        // SAFETY: x + 8 <= width 时读 8×4 字节；out 至少 width（向上取偶）字节。
        unsafe {
            while x + 8 <= width {
                luma8_avx2(row.as_ptr().add(x * 4), out.as_mut_ptr().add(x));
                x += 8;
            }
        }
    }
    while x < width {
        out[x] = luma_pixel(&row[x * 4..]);
        x += 1;
    }
    // 奇数宽度：复制最后一列。
    while x < out.len() {
        out[x] = out[width - 1];
        x += 1;
    }
}

/// 一行色度（两行像素 → 一行 UV）。
#[cfg(target_arch = "x86_64")]
fn chroma_row(row0: &[u8], row1: &[u8], width: usize, out: &mut [u8]) {
    let pairs = width / 2;
    let mut cx = 0usize;
    if pairs >= 4 && is_avx2() {
        // SAFETY: `uv8_avx2` 一次处理 4 对（两行各 8 像素），
        // cx + 4 <= pairs 才进入。
        unsafe {
            while cx + 4 <= pairs {
                let x = cx * 2;
                uv8_avx2(
                    row0.as_ptr().add(x * 4),
                    row1.as_ptr().add(x * 4),
                    out.as_mut_ptr().add(cx * 2),
                );
                cx += 4;
            }
        }
    }
    while cx < pairs {
        let x = cx * 2;
        let (u, v) = chroma_pixels(
            &row0[x * 4..],
            &row0[(x + 1) * 4..],
            &row1[x * 4..],
            &row1[(x + 1) * 4..],
        );
        out[cx * 2] = u;
        out[cx * 2 + 1] = v;
        cx += 1;
    }
    // 奇数宽度：最后一对由同一个像素重复两次组成。
    if width & 1 == 1 {
        let last = width - 1;
        let (u, v) = chroma_pixels(
            &row0[last * 4..],
            &row0[last * 4..],
            &row1[last * 4..],
            &row1[last * 4..],
        );
        out[pairs * 2] = u;
        out[pairs * 2 + 1] = v;
    }
}

// 非 x86_64 平台（本项目只发布 Windows，这里只为让 `cargo check` 在别处也能过）。
#[cfg(not(target_arch = "x86_64"))]
fn luma_row(row: &[u8], width: usize, out: &mut [u8]) {
    for x in 0..width {
        out[x] = luma_pixel(&row[x * 4..]);
    }
    for x in width..out.len() {
        out[x] = out[width - 1];
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn chroma_row(row0: &[u8], row1: &[u8], width: usize, out: &mut [u8]) {
    for cx in 0..width / 2 {
        let x = cx * 2;
        let (u, v) = chroma_pixels(
            &row0[x * 4..],
            &row0[(x + 1) * 4..],
            &row1[x * 4..],
            &row1[(x + 1) * 4..],
        );
        out[cx * 2] = u;
        out[cx * 2 + 1] = v;
    }
}

#[cfg(target_arch = "x86_64")]
fn is_avx2() -> bool {
    std::is_x86_feature_detected!("avx2")
}

#[inline]
fn luma_pixel(px: &[u8]) -> u8 {
    let b = px[0] as i32;
    let g = px[1] as i32;
    let r = px[2] as i32;
    (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16).clamp(0, 255) as u8
}

/// 2×2 块的平均色度。逐像素先加偏移再求和、最后算术右移，
/// 与 AVX2 版本逐位一致（整数除法的舍入方向不同会导致 ±1 的差异）。
#[inline]
fn chroma_pixels(p00: &[u8], p01: &[u8], p10: &[u8], p11: &[u8]) -> (u8, u8) {
    let mut u = 0i32;
    let mut v = 0i32;
    for px in [p00, p01, p10, p11] {
        let b = px[0] as i32;
        let g = px[1] as i32;
        let r = px[2] as i32;
        u += ((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128;
        v += ((112 * r - 102 * g - 10 * b + 128) >> 8) + 128;
    }
    ((u >> 2).clamp(0, 255) as u8, (v >> 2).clamp(0, 255) as u8)
}

// ---------------------------------------------------------------- AVX2

/// 8 个像素的亮度，一次写 8 字节。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn luma8_avx2(src: *const u8, out: *mut u8) {
    use std::arch::x86_64::*;

    let v = _mm256_loadu_si256(src as *const __m256i);
    let mask = _mm256_set1_epi32(0xff);
    let b = _mm256_and_si256(v, mask);
    let g = _mm256_and_si256(_mm256_srli_epi32::<8>(v), mask);
    let r = _mm256_and_si256(_mm256_srli_epi32::<16>(v), mask);

    let mut y = _mm256_mullo_epi32(r, _mm256_set1_epi32(47));
    y = _mm256_add_epi32(y, _mm256_mullo_epi32(g, _mm256_set1_epi32(157)));
    y = _mm256_add_epi32(y, _mm256_mullo_epi32(b, _mm256_set1_epi32(16)));
    y = _mm256_add_epi32(y, _mm256_set1_epi32(128));
    y = _mm256_srai_epi32::<8>(y);
    y = _mm256_add_epi32(y, _mm256_set1_epi32(16));

    let lo = _mm256_castsi256_si128(y);
    let hi = _mm256_extracti128_si256(y, 1);
    let p16 = _mm_packus_epi32(lo, hi);
    let p8 = _mm_packus_epi16(p16, p16);
    _mm_storel_epi64(out as *mut __m128i, p8);
}

/// 4 对色度（两行各 8 像素）→ 8 字节 UV，U/V 交错。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn uv8_avx2(row0: *const u8, row1: *const u8, out: *mut u8) {
    use std::arch::x86_64::*;

    let (u0, v0) = uv8_row(row0);
    let (u1, v1) = uv8_row(row1);
    let su = _mm256_add_epi32(u0, u1);
    let sv = _mm256_add_epi32(v0, v1);

    // 每 4 个像素（2×2）取平均：先两两相加，再除 4。
    let u4 = pair_avg(su);
    let v4 = pair_avg(sv);
    let u16 = _mm_packus_epi32(u4, u4);
    let v16 = _mm_packus_epi32(v4, v4);
    let u8v = _mm_packus_epi16(u16, u16);
    let v8v = _mm_packus_epi16(v16, v16);
    let uv = _mm_unpacklo_epi8(u8v, v8v);
    _mm_storel_epi64(out as *mut __m128i, uv);
}

/// 8 个像素的 U/V（i32 通道，已按公式算好但还没减半）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn uv8_row(src: *const u8) -> (std::arch::x86_64::__m256i, std::arch::x86_64::__m256i) {
    use std::arch::x86_64::*;

    let v = _mm256_loadu_si256(src as *const __m256i);
    let mask = _mm256_set1_epi32(0xff);
    let b = _mm256_and_si256(v, mask);
    let g = _mm256_and_si256(_mm256_srli_epi32::<8>(v), mask);
    let r = _mm256_and_si256(_mm256_srli_epi32::<16>(v), mask);

    let mut u = _mm256_mullo_epi32(b, _mm256_set1_epi32(112));
    u = _mm256_sub_epi32(u, _mm256_mullo_epi32(r, _mm256_set1_epi32(26)));
    u = _mm256_sub_epi32(u, _mm256_mullo_epi32(g, _mm256_set1_epi32(87)));
    u = _mm256_add_epi32(u, _mm256_set1_epi32(128));
    u = _mm256_srai_epi32::<8>(u);
    u = _mm256_add_epi32(u, _mm256_set1_epi32(128));

    let mut w = _mm256_mullo_epi32(r, _mm256_set1_epi32(112));
    w = _mm256_sub_epi32(w, _mm256_mullo_epi32(g, _mm256_set1_epi32(102)));
    w = _mm256_sub_epi32(w, _mm256_mullo_epi32(b, _mm256_set1_epi32(10)));
    w = _mm256_add_epi32(w, _mm256_set1_epi32(128));
    w = _mm256_srai_epi32::<8>(w);
    w = _mm256_add_epi32(w, _mm256_set1_epi32(128));

    (u, w)
}

/// 8 个 i32 → 相邻两两求和、再右移 2（除以 4），取前 4 个结果。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn pair_avg(v: std::arch::x86_64::__m256i) -> std::arch::x86_64::__m128i {
    use std::arch::x86_64::*;

    let h = _mm256_hadd_epi32(v, v);
    let lo = _mm256_castsi256_si128(h);
    let hi = _mm256_extracti128_si256(h, 1);
    let four = _mm_unpacklo_epi64(lo, hi);
    _mm_srai_epi32::<2>(four)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: usize, height: usize, b: u8, g: u8, r: u8) -> Vec<u8> {
        let mut src = Vec::with_capacity(width * height * 4);
        for _ in 0..width * height {
            src.extend_from_slice(&[b, g, r, 255]);
        }
        src
    }

    #[test]
    fn black_is_bt709_limited() {
        let src = solid(16, 16, 0, 0, 0);
        let mut dst = Vec::new();
        let (w, h) = bgra_to_nv12(&src, 16, 16, &mut dst);
        assert_eq!((w, h), (16, 16));
        assert_eq!(dst.len(), 16 * 16 * 3 / 2);
        // 黑：Y=16，U=V=128。
        assert!(dst[..16 * 16].iter().all(|&y| y == 16));
        assert!(dst[16 * 16..].iter().step_by(2).all(|&u| u == 128));
        assert!(dst[16 * 16 + 1..].iter().step_by(2).all(|&v| v == 128));
    }

    #[test]
    fn white_and_primaries_stay_in_range() {
        for (b, g, r) in [(255, 255, 255), (255, 0, 0), (0, 255, 0), (0, 0, 255)] {
            let src = solid(8, 8, b, g, r);
            let mut dst = Vec::new();
            bgra_to_nv12(&src, 8, 8, &mut dst);
            // 纯色每一块色度都应该一致。
            let uv = &dst[64..];
            assert!(uv.iter().step_by(2).all(|&u| u == uv[0]));
            assert!(uv[1..].iter().step_by(2).all(|&v| v == uv[1]));
        }
        let src = solid(8, 8, 255, 255, 255);
        let mut dst = Vec::new();
        bgra_to_nv12(&src, 8, 8, &mut dst);
        assert!(
            dst[..64].iter().all(|&y| (230..=236).contains(&y)),
            "白点 Y={}",
            dst[0]
        );
    }

    #[test]
    fn odd_dimensions_are_padded() {
        let src = solid(3, 3, 10, 20, 30);
        let mut dst = Vec::new();
        let (w, h) = bgra_to_nv12(&src, 3, 3, &mut dst);
        assert_eq!((w, h), (4, 4));
        assert_eq!(dst.len(), 4 * 4 * 3 / 2);
        // 补出来的第 4 列与第 3 列相同。
        assert_eq!(dst[3], dst[2]);
        assert_eq!(dst[7], dst[6]);
    }

    #[test]
    fn avx2_matches_scalar() {
        // 覆盖向量块边界（8/16/32 像素）、奇数尺寸和半行剩余。
        for (width, height) in [
            (8usize, 4usize),
            (16, 2),
            (17, 3),
            (31, 5),
            (32, 8),
            (33, 9),
            (37, 21),
            (63, 6),
            (64, 10),
            (65, 7),
        ] {
            let mut src = Vec::with_capacity(width * height * 4);
            let mut seed = 0x1234_5678u32 ^ (width as u32) << 16 ^ height as u32;
            for _ in 0..width * height {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                src.extend_from_slice(&[
                    (seed >> 24) as u8,
                    (seed >> 16) as u8,
                    (seed >> 8) as u8,
                    255,
                ]);
            }
            let mut simd = Vec::new();
            let mut scalar = Vec::new();
            let a = bgra_to_nv12(&src, width, height, &mut simd);
            let b = scalar_to_nv12(&src, width, height, &mut scalar);
            assert_eq!(a, b, "{width}x{height}");
            assert_eq!(simd, scalar, "{width}x{height} 的 SIMD 与标量结果不一致");
        }
    }

    /// 转换基准：`cargo test -p screenwall-agent --release nv12_bench -- --ignored --nocapture`
    ///
    /// 同时报融合版（生产路径）与两趟版（旧路径）的耗时，方便看融合省了多少。
    #[test]
    #[ignore = "性能基准；用 --ignored 手动跑"]
    fn nv12_bench() {
        for (name, width, height) in [
            ("1080p", 1920usize, 1080usize),
            ("1440p", 2560, 1440),
            ("4K", 3840, 2160),
        ] {
            let mut src = vec![0u8; width * height * 4];
            for y in 0..height {
                for x in 0..width {
                    let index = (y * width + x) * 4;
                    src[index] = (x % 256) as u8;
                    src[index + 1] = (y % 256) as u8;
                    src[index + 2] = ((x + y) % 256) as u8;
                    src[index + 3] = 255;
                }
            }
            let rounds = 20;
            let reps = 5;
            let mut full = Vec::new();
            let mut two_pass = vec![0u8; nv12_len(width, height)];
            let mut luma = vec![0u8; width * height];

            // 预热，顺带把缓冲区扩到位。
            bgra_to_nv12(&src, width, height, &mut full);

            // 机器上还有别的负载，单次测量抖动大，取 5 轮里最快的一轮。
            let mut blocked_ms = f64::MAX;
            let mut two_pass_ms = f64::MAX;
            for _ in 0..reps {
                let started = std::time::Instant::now();
                for _ in 0..rounds {
                    bgra_to_nv12(&src, width, height, &mut full);
                }
                let ms = started.elapsed().as_secs_f64() * 1000.0 / rounds as f64;
                blocked_ms = blocked_ms.min(ms);
                std::hint::black_box(&full);

                let started = std::time::Instant::now();
                for _ in 0..rounds {
                    for y in 0..height {
                        let row = &src[y * width * 4..];
                        luma_row(row, width, &mut luma[y * width..(y + 1) * width]);
                    }
                    let uv = width * height;
                    for cy in 0..height / 2 {
                        let row0 = &src[cy * 2 * width * 4..];
                        let row1 = &src[(cy * 2 + 1) * width * 4..];
                        chroma_row(
                            row0,
                            row1,
                            width,
                            &mut two_pass[uv + cy * width..uv + (cy + 1) * width],
                        );
                    }
                }
                let ms = started.elapsed().as_secs_f64() * 1000.0 / rounds as f64;
                two_pass_ms = two_pass_ms.min(ms);
                std::hint::black_box((&luma, &two_pass));
            }

            println!(
                "{name} {width}x{height}: 分块 {blocked_ms:.2} ms/帧，两趟 {two_pass_ms:.2} ms/帧"
            );
        }
    }

    /// 测试用的纯标量实现（与 AVX2 对拍）。
    fn scalar_to_nv12(
        src: &[u8],
        width: usize,
        height: usize,
        dst: &mut Vec<u8>,
    ) -> (usize, usize) {
        let (w2, h2) = padded_dims(width, height);
        dst.clear();
        dst.resize(w2 * h2 * 3 / 2, 0);
        for y in 0..h2 {
            let sy = y.min(height - 1);
            for x in 0..w2 {
                let sx = x.min(width - 1);
                dst[y * w2 + x] = luma_pixel(&src[(sy * width + sx) * 4..]);
            }
        }
        let uv = w2 * h2;
        for cy in 0..h2 / 2 {
            let y0 = (cy * 2).min(height - 1);
            let y1 = (cy * 2 + 1).min(height - 1);
            for cx in 0..w2 / 2 {
                let x0 = (cx * 2).min(width - 1);
                let x1 = (cx * 2 + 1).min(width - 1);
                let (u, v) = chroma_pixels(
                    &src[(y0 * width + x0) * 4..],
                    &src[(y0 * width + x1) * 4..],
                    &src[(y1 * width + x0) * 4..],
                    &src[(y1 * width + x1) * 4..],
                );
                dst[uv + cy * w2 + cx * 2] = u;
                dst[uv + cy * w2 + cx * 2 + 1] = v;
            }
        }
        (w2, h2)
    }
}
