//! Thinking Budget 整流器
//!
//! 用于自动修复 Anthropic API 中因 thinking budget 约束导致的请求错误。
//! 当上游 API 返回 budget_tokens 相关错误时，系统会自动调整 budget 参数并重试。

use super::types::RectifierConfig;
use serde_json::Value;

/// 最大 thinking budget tokens
const MAX_THINKING_BUDGET: u64 = 32000;

/// 最大 max_tokens 值
const MAX_TOKENS_VALUE: u64 = 64000;

/// max_tokens 必须大于 budget_tokens
const MIN_MAX_TOKENS_FOR_BUDGET: u64 = MAX_THINKING_BUDGET + 1;

/// Budget 整流结果
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BudgetRectifySnapshot {
    /// max_tokens
    pub max_tokens: Option<u64>,
    /// thinking.type
    pub thinking_type: Option<String>,
    /// thinking.budget_tokens
    pub thinking_budget_tokens: Option<u64>,
}

/// Budget 整流结果
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BudgetRectifyResult {
    /// 是否应用了整流
    pub applied: bool,
    /// 整流前快照
    pub before: BudgetRectifySnapshot,
    /// 整流后快照
    pub after: BudgetRectifySnapshot,
}

/// 检测是否需要触发 thinking budget 整流器
///
/// 检测条件：error message 同时包含 `budget_tokens` + `thinking` 相关约束
pub fn should_rectify_thinking_budget(
    error_message: Option<&str>,
    config: &RectifierConfig,
) -> bool {
    // 检查总开关
    if !config.enabled {
        return false;
    }
    // 检查子开关
    if !config.request_thinking_budget {
        return false;
    }

    let Some(msg) = error_message else {
        return false;
    };
    let lower = msg.to_lowercase();

    // 与 CCH 对齐：仅在包含 budget_tokens + thinking + 1024 约束时触发
    let has_budget_tokens_reference =
        lower.contains("budget_tokens") || lower.contains("budget tokens");
    let has_thinking_reference = lower.contains("thinking");
    let has_1024_constraint = lower.contains("greater than or equal to 1024")
        || lower.contains(">= 1024")
        || (lower.contains("1024") && lower.contains("input should be"));
    if has_budget_tokens_reference && has_thinking_reference && has_1024_constraint {
        return true;
    }

    false
}

/// 对请求体执行 budget 整流
///
/// 整流动作：
/// - `thinking.type = "enabled"`
/// - `thinking.budget_tokens = min(32000, max_tokens_ceiling - 1)`（≥1024 兜底）
/// - 如果 `max_tokens < 32001`，设为 `min(64000, max_tokens_ceiling)`
///
/// `max_tokens_ceiling` 由调用方按供应商传入：
/// - 火山引擎 Ark 等受限网关传 `32768`（或更紧的 2048 等）
/// - 官方 Anthropic / 中转服务传 `u64::MAX`（与旧实现行为完全一致）
pub fn rectify_thinking_budget(body: &mut Value, max_tokens_ceiling: u64) -> BudgetRectifyResult {
    let before = snapshot_budget(body);

    // 与 CCH 对齐：adaptive 请求不改写
    if before.thinking_type.as_deref() == Some("adaptive") {
        return BudgetRectifyResult {
            applied: false,
            before: before.clone(),
            after: before,
        };
    }

    // 与 CCH 对齐：缺少/非法 thinking 时自动创建后再整流
    if !body.get("thinking").is_some_and(Value::is_object) {
        body["thinking"] = Value::Object(serde_json::Map::new());
    }

    let Some(thinking) = body.get_mut("thinking").and_then(|t| t.as_object_mut()) else {
        return BudgetRectifyResult {
            applied: false,
            before: before.clone(),
            after: before,
        };
    };

    // 在受限 ceiling 下 max_tokens 会被钳小，budget_tokens 必须严格 < max_tokens
    // （Anthropic 400 on `budget_tokens >= max_tokens`）。target_max_tokens
    // 必须先确定再算 budget，否则会出现 budget=32000 但 max_tokens=2048 的非法态。
    let target_max_tokens = MAX_TOKENS_VALUE.min(max_tokens_ceiling);
    let clamped_budget = if target_max_tokens > 1 {
        MAX_THINKING_BUDGET
            .min(target_max_tokens.saturating_sub(1))
            .max(1024)
    } else {
        1024
    };

    thinking.insert("type".to_string(), Value::String("enabled".to_string()));
    thinking.insert(
        "budget_tokens".to_string(),
        Value::Number(clamped_budget.into()),
    );

    if before.max_tokens.is_none() || before.max_tokens < Some(MIN_MAX_TOKENS_FOR_BUDGET) {
        body["max_tokens"] = Value::Number(target_max_tokens.into());
    }

    let after = snapshot_budget(body);
    BudgetRectifyResult {
        applied: before != after,
        before,
        after,
    }
}

fn snapshot_budget(body: &Value) -> BudgetRectifySnapshot {
    let max_tokens = body.get("max_tokens").and_then(|v| v.as_u64());
    let thinking = body.get("thinking").and_then(|t| t.as_object());
    let thinking_type = thinking
        .and_then(|t| t.get("type"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    let thinking_budget_tokens = thinking
        .and_then(|t| t.get("budget_tokens"))
        .and_then(|v| v.as_u64());
    BudgetRectifySnapshot {
        max_tokens,
        thinking_type,
        thinking_budget_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn enabled_config() -> RectifierConfig {
        RectifierConfig {
            enabled: true,
            request_thinking_signature: true,
            request_thinking_budget: true,
            request_media_fallback: true,
            request_media_heuristic: true,
        }
    }

    fn budget_disabled_config() -> RectifierConfig {
        RectifierConfig {
            enabled: true,
            request_thinking_signature: true,
            request_thinking_budget: false,
            request_media_fallback: true,
            request_media_heuristic: true,
        }
    }

    fn master_disabled_config() -> RectifierConfig {
        RectifierConfig {
            enabled: false,
            request_thinking_signature: true,
            request_thinking_budget: true,
            request_media_fallback: true,
            request_media_heuristic: true,
        }
    }

    // ==================== should_rectify_thinking_budget 测试 ====================

    #[test]
    fn test_detect_budget_tokens_thinking_error() {
        assert!(should_rectify_thinking_budget(
            Some("thinking.budget_tokens: Input should be greater than or equal to 1024"),
            &enabled_config()
        ));
    }

    #[test]
    fn test_detect_budget_tokens_max_tokens_error() {
        assert!(!should_rectify_thinking_budget(
            Some("budget_tokens must be less than max_tokens"),
            &enabled_config()
        ));
    }

    #[test]
    fn test_detect_budget_tokens_1024_error() {
        assert!(!should_rectify_thinking_budget(
            Some("budget_tokens: value must be at least 1024"),
            &enabled_config()
        ));
    }

    #[test]
    fn test_detect_budget_tokens_with_thinking_and_1024_error() {
        assert!(should_rectify_thinking_budget(
            Some("thinking budget_tokens must be >= 1024"),
            &enabled_config()
        ));
    }

    #[test]
    fn test_no_trigger_for_unrelated_error() {
        assert!(!should_rectify_thinking_budget(
            Some("Request timeout"),
            &enabled_config()
        ));
        assert!(!should_rectify_thinking_budget(None, &enabled_config()));
    }

    #[test]
    fn test_disabled_budget_config() {
        assert!(!should_rectify_thinking_budget(
            Some("thinking.budget_tokens: Input should be greater than or equal to 1024"),
            &budget_disabled_config()
        ));
    }

    #[test]
    fn test_master_disabled() {
        assert!(!should_rectify_thinking_budget(
            Some("thinking.budget_tokens: Input should be greater than or equal to 1024"),
            &master_disabled_config()
        ));
    }

    // ==================== rectify_thinking_budget 测试 ====================

    #[test]
    fn test_rectify_budget_basic() {
        let mut body = json!({
            "model": "claude-test",
            "thinking": { "type": "enabled", "budget_tokens": 512 },
            "max_tokens": 1024
        });

        let result = rectify_thinking_budget(&mut body, u64::MAX);

        assert!(result.applied);
        assert_eq!(result.before.thinking_type.as_deref(), Some("enabled"));
        assert_eq!(result.after.thinking_type.as_deref(), Some("enabled"));
        assert_eq!(result.before.thinking_budget_tokens, Some(512));
        assert_eq!(
            result.after.thinking_budget_tokens,
            Some(MAX_THINKING_BUDGET)
        );
        assert_eq!(result.before.max_tokens, Some(1024));
        assert_eq!(result.after.max_tokens, Some(MAX_TOKENS_VALUE));
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], MAX_THINKING_BUDGET);
        assert_eq!(body["max_tokens"], MAX_TOKENS_VALUE);
    }

    #[test]
    fn test_rectify_budget_skips_adaptive() {
        let mut body = json!({
            "model": "claude-test",
            "thinking": { "type": "adaptive", "budget_tokens": 512 },
            "max_tokens": 1024
        });

        let result = rectify_thinking_budget(&mut body, u64::MAX);

        assert!(!result.applied);
        assert_eq!(result.before, result.after);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["budget_tokens"], 512);
        assert_eq!(body["max_tokens"], 1024);
    }

    #[test]
    fn test_rectify_budget_preserves_large_max_tokens() {
        let mut body = json!({
            "model": "claude-test",
            "thinking": { "type": "enabled", "budget_tokens": 512 },
            "max_tokens": 100000
        });

        let result = rectify_thinking_budget(&mut body, u64::MAX);

        assert!(result.applied);
        assert_eq!(result.before.max_tokens, Some(100000));
        assert_eq!(result.after.max_tokens, Some(100000));
        assert_eq!(body["max_tokens"], 100000);
    }

    #[test]
    fn test_rectify_budget_creates_thinking_object_when_missing() {
        let mut body = json!({
            "model": "claude-test",
            "max_tokens": 1024
        });

        let result = rectify_thinking_budget(&mut body, u64::MAX);

        assert!(result.applied);
        assert_eq!(result.before.thinking_type, None);
        assert_eq!(result.after.thinking_type.as_deref(), Some("enabled"));
        assert_eq!(
            result.after.thinking_budget_tokens,
            Some(MAX_THINKING_BUDGET)
        );
        assert_eq!(result.after.max_tokens, Some(MAX_TOKENS_VALUE));
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], MAX_THINKING_BUDGET);
        assert_eq!(body["max_tokens"], MAX_TOKENS_VALUE);
    }

    #[test]
    fn test_rectify_budget_no_max_tokens() {
        let mut body = json!({
            "model": "claude-test",
            "thinking": { "type": "enabled", "budget_tokens": 512 }
        });

        let result = rectify_thinking_budget(&mut body, u64::MAX);

        assert!(result.applied);
        assert_eq!(result.before.max_tokens, None);
        assert_eq!(result.after.max_tokens, Some(MAX_TOKENS_VALUE));
        assert_eq!(body["max_tokens"], MAX_TOKENS_VALUE);
    }

    #[test]
    fn test_rectify_budget_normalizes_non_enabled_type() {
        let mut body = json!({
            "model": "claude-test",
            "thinking": { "type": "disabled", "budget_tokens": 512 },
            "max_tokens": 1024
        });

        let result = rectify_thinking_budget(&mut body, u64::MAX);

        assert!(result.applied);
        assert_eq!(result.before.thinking_type.as_deref(), Some("disabled"));
        assert_eq!(result.after.thinking_type.as_deref(), Some("enabled"));
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], MAX_THINKING_BUDGET);
        assert_eq!(body["max_tokens"], MAX_TOKENS_VALUE);
    }

    #[test]
    fn test_rectify_budget_no_change_when_already_valid() {
        let mut body = json!({
            "model": "claude-test",
            "thinking": { "type": "enabled", "budget_tokens": 32000 },
            "max_tokens": 64001
        });

        let result = rectify_thinking_budget(&mut body, u64::MAX);

        assert!(!result.applied);
        assert_eq!(result.before, result.after);
        assert_eq!(body["thinking"]["budget_tokens"], 32000);
        assert_eq!(body["max_tokens"], 64001);
    }

    // ==================== ceiling 钳制测试（Volcengine Ark 等受限网关） ====================

    #[test]
    fn test_rectify_budget_caps_max_tokens_at_volcengine_ceiling() {
        // 模拟火山方舟网关：ceiling=32768。客户端发 max_tokens=1024，整流器
        // 不再把 max_tokens 抬到 64000，而是钳到 32768；budget_tokens=32000
        // 仍然 < 32768（满足 Anthropic 不变量 budget < max_tokens）。
        let mut body = json!({
            "model": "claude-test",
            "thinking": { "type": "enabled", "budget_tokens": 512 },
            "max_tokens": 1024
        });

        let result = rectify_thinking_budget(&mut body, 32768);

        assert!(result.applied);
        assert_eq!(result.after.max_tokens, Some(32768));
        assert_eq!(body["max_tokens"], 32768);
        // 32000 < 32768 严格小于，符合 Anthropic `budget_tokens < max_tokens` 约束
        assert_eq!(body["thinking"]["budget_tokens"], 32000);
        assert!(
            body["thinking"]["budget_tokens"].as_u64().unwrap()
                < body["max_tokens"].as_u64().unwrap()
        );
    }

    #[test]
    fn test_rectify_budget_caps_thinking_budget_when_ceiling_smaller_than_default() {
        // 极小 ceiling（未来其他受限网关可能）：ceiling=2048。max_tokens 钳到
        // 2048，budget_tokens 必须严格 < 2048，且 ≥ 1024（Anthropic 下限）。
        let mut body = json!({
            "model": "claude-test",
            "thinking": { "type": "enabled", "budget_tokens": 512 },
            "max_tokens": 1024
        });

        let result = rectify_thinking_budget(&mut body, 2048);

        assert!(result.applied);
        assert_eq!(body["max_tokens"], 2048);
        // budget 兜底：min(32000, 2048-1)=2047 与 max(1024) 取 = 2047
        // （2047 < 2048 严格小于约束）
        let budget = body["thinking"]["budget_tokens"].as_u64().unwrap();
        assert_eq!(budget, 2047);
        assert!(budget < body["max_tokens"].as_u64().unwrap());
        assert!(budget >= 1024);
    }
}
