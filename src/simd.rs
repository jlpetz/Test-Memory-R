use std::arch::x86_64::*;

// SIMD capability detection
pub fn detect_simd_capabilities() -> String {
    let mut capabilities = Vec::new();

    if is_x86_feature_detected!("sse") {
        capabilities.push("SSE");
    }
    if is_x86_feature_detected!("sse2") {
        capabilities.push("SSE2");
    }
    if is_x86_feature_detected!("sse3") {
        capabilities.push("SSE3");
    }
    if is_x86_feature_detected!("sse4.1") {
        capabilities.push("SSE4.1");
    }
    if is_x86_feature_detected!("sse4.2") {
        capabilities.push("SSE4.2");
    }
    if is_x86_feature_detected!("avx") {
        capabilities.push("AVX");
    }
    if is_x86_feature_detected!("avx2") {
        capabilities.push("AVX2");
    }
    if is_x86_feature_detected!("avx512f") {
        capabilities.push("AVX-512F");
    }
    if is_x86_feature_detected!("avx512bw") {
        capabilities.push("AVX-512BW");
    }
    if is_x86_feature_detected!("avx512vl") {
        capabilities.push("AVX-512VL");
    }

    if capabilities.is_empty() {
        "None detected".to_string()
    } else {
        capabilities.join(", ")
    }
}