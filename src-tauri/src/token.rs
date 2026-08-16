use rand::Rng;

/// 生成聚合器对外令牌: cpm-<32位十六进制>
pub fn generate_token() -> String {
    let mut rng = rand::thread_rng();
    let hex: String = (0..32).map(|_| format!("{:x}", rng.gen_range(0..16u32))).collect();
    format!("cpm-{}", hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_format() {
        let t = generate_token();
        assert!(t.starts_with("cpm-"));
        assert_eq!(t.len(), 4 + 32);
        let t2 = generate_token();
        assert_ne!(t, t2);
    }
}
