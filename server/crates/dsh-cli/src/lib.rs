//! dsh-cli — 模块占位（M0 脚手架）。按 docs/design-modules 对应规格在后续里程碑实现。
#![allow(dead_code)]

/// 占位函数：模块尚未实现。
pub fn placeholder() -> &'static str {
    "module not yet implemented"
}

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder_ok() {
        assert_eq!(super::placeholder(), "module not yet implemented");
    }
}
