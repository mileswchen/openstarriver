//! WI-200: output-side degenerate-loop breaker.
//!
//! A model stuck in an n-gram repetition loop (observed live: glm-5.3-flash
//! repeated the same sentence for 8h+ on a long poisoned context —
//! 「…✓。写实现。」 over and over, never emitting a tool call) burns the
//! stream until an upstream idle timeout kills it ~30s per attempt, and the
//! looped text then cements itself into the history. This monitor watches the
//! STREAMED text as it arrives and trips once the tail window is dominated by
//! the same sentence repeating, so the loop can ABORT the stream immediately
//! (a `DEGENERATE_LOOP` failure — retryable per the kernel RetryPolicy) and
//! re-ask with a diverged strategy instead.

/// Tail window of accumulated stream text the detector reasons about. Long
/// enough to hold several repetitions of a typical loop sentence; short
/// enough to stay O(1)-ish per feed.
const WINDOW_CHARS: usize = 2_000;

/// A sentence of at least this many code points trips after only a handful
/// of repetitions — long sentences don't repeat five times by accident.
const MIN_SENTENCE_CHARS: usize = 12;

/// Repetitions of a LONG (≥ MIN_SENTENCE_CHARS) trailing sentence within the
/// window that trip the breaker.
const TRIP_REPETITIONS: usize = 5;

/// Short sentences (2..12 chars — the observed loop shape「写实现。」is 4)
/// repeat harmlessly in normal output, so they need an avalanche before the
/// breaker fires: dozens of copies inside the 2k window is a loop, not style.
const SHORT_SENTENCE_MIN_CHARS: usize = 2;
const SHORT_TRIP_REPETITIONS: usize = 40;

/// WI-210 (BUG-152): paraphrase channel. The second incident shape restates
/// the same status sentence with small lexical variations —「正在编写实现。
/// 」「现在正在编写实现。」「正在编写实现代码。」「我现在开始编写实现代码。
/// 」— so exact-match counting never reaches its bar and the stream burns
/// until the transport dies. Two sentences are COUSINS when their character
/// multiset Dice coefficient reaches [`PARAPHRASE_SIMILARITY`]; a sentence
/// with at least [`PARAPHRASE_COUSINS`] cousins is a near copy, and enough
/// near copies inside the window is a loop even though no two sentences are
/// byte-identical. The bar sits deliberately high (10 near copies) because a
/// false trip costs a retry — long templated enumerations can still trip it,
/// but the trip is recoverable while an unterminated loop is not.
const PARAPHRASE_MIN_CHARS: usize = 6;
const PARAPHRASE_SIMILARITY: f32 = 0.75;
const PARAPHRASE_COUSINS: usize = 2;
const PARAPHRASE_TRIP_SENTENCES: usize = 10;
/// Cap on how many trailing sentences the O(n²) pairwise pass covers, so a
/// window of tiny sentences can't blow up the per-feed cost.
const PARAPHRASE_ANALYZED_SENTENCES: usize = 24;

/// Sentence terminators for the split. CJK terminators lead because the
/// observed loops are Chinese-first; ASCII and newline cover code/prose.
const SENTENCE_TERMINATORS: [char; 8] = ['。', '！', '？', '.', '!', '?', '\n', '；'];

#[derive(Default)]
pub struct DegenerateLoopMonitor {
    tail: String,
}

impl DegenerateLoopMonitor {
    /// Feed one streamed text delta. Returns true when the repetition loop
    /// trips (the caller must abort the stream).
    pub fn feed(&mut self, delta: &str) -> bool {
        if delta.is_empty() {
            return false;
        }
        self.tail.push_str(delta);
        let total = self.tail.chars().count();
        if total > WINDOW_CHARS {
            let skip = total - WINDOW_CHARS;
            self.tail = self.tail.chars().skip(skip).collect();
        }
        Self::tripped(&self.tail)
    }

    /// Pure predicate over a text window. Two channels: the exact-match bar
    /// (the first incident's shape) and the WI-210 paraphrase channel (the
    /// second's — same sentence, varied wording).
    fn tripped(window: &str) -> bool {
        if Self::exact_repetition_tripped(window) {
            return true;
        }
        Self::paraphrase_tripped(window)
    }

    /// The exact-match bar: does the FINAL sentence repeat verbatim enough
    /// times? The bar scales with sentence length — long sentences trip
    /// after 5 copies, short ones (the observed loop shape) need an
    /// avalanche, 1-char tails never trip.
    fn exact_repetition_tripped(window: &str) -> bool {
        let last = match last_sentence(window) {
            Some(sentence) => sentence,
            None => return false,
        };
        let len = last.chars().count();
        let required = if len >= MIN_SENTENCE_CHARS {
            TRIP_REPETITIONS
        } else if len >= SHORT_SENTENCE_MIN_CHARS {
            SHORT_TRIP_REPETITIONS
        } else {
            usize::MAX
        };
        window.matches(&last).count() >= required
    }

    /// The paraphrase bar: enough of the trailing sentences sit in
    /// similarity families (each with ≥ [`PARAPHRASE_COUSINS`] cousins) that
    /// the window is restating itself instead of producing new content.
    /// Armed only when at least one sentence repeats VERBATIM in the window:
    /// loops recycle their sentences, while templated enumerations (the
    /// false-positive shape — 200 near-identical list items differing by a
    /// digit) emit each instance exactly once.
    fn paraphrase_tripped(window: &str) -> bool {
        let mut sentences: Vec<&str> = split_sentences(window)
            .into_iter()
            .filter(|sentence| sentence.chars().count() >= PARAPHRASE_MIN_CHARS)
            .collect();
        if sentences.len() > PARAPHRASE_ANALYZED_SENTENCES {
            let skip = sentences.len() - PARAPHRASE_ANALYZED_SENTENCES;
            sentences.drain(..skip);
        }
        if sentences.len() < PARAPHRASE_TRIP_SENTENCES {
            return false;
        }
        let verbatim_repeat = |needle: &str| window.matches(needle).count() >= 2;
        if !sentences.iter().any(|sentence| verbatim_repeat(sentence)) {
            return false;
        }
        let profiles: Vec<Vec<char>> = sentences
            .iter()
            .map(|sentence| {
                let mut chars: Vec<char> = sentence.chars().collect();
                chars.sort_unstable();
                chars
            })
            .collect();
        let mut cousins = vec![0usize; profiles.len()];
        for i in 0..profiles.len() {
            for j in i + 1..profiles.len() {
                if dice_similarity(&profiles[i], &profiles[j]) >= PARAPHRASE_SIMILARITY {
                    cousins[i] += 1;
                    cousins[j] += 1;
                }
            }
        }
        let near_copies = cousins
            .iter()
            .filter(|count| **count >= PARAPHRASE_COUSINS)
            .count();
        near_copies >= PARAPHRASE_TRIP_SENTENCES
    }
}

/// Multiset Dice coefficient over the sorted char vectors: 2·|A∩B|/(|A|+|B|).
/// Both inputs MUST be sorted ascending — the caller sorts each sentence's
/// chars once and reuses the profile across all pairwise comparisons.
fn dice_similarity(a: &[char], b: &[char]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let mut shared = 0usize;
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                shared += 1;
                i += 1;
                j += 1;
            }
        }
    }
    (2 * shared) as f32 / (a.len() + b.len()) as f32
}

fn split_sentences(text: &str) -> Vec<&str> {
    text.split(SENTENCE_TERMINATORS)
        .map(str::trim)
        .filter(|sentence| !sentence.is_empty())
        .collect()
}

/// The sentence to repetition-check. Mid-sentence tails take the trailing
/// partial; when the window ends ON a terminator (the normal shape — loops
/// repeat whole sentences), take the sentence that terminator just COMPLETED,
/// otherwise the empty tail would silently disable the check.
fn last_sentence(text: &str) -> Option<&str> {
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    let (last_idx, last_ch) = trimmed
        .char_indices()
        .rev()
        .find(|(_, ch)| SENTENCE_TERMINATORS.contains(ch))?;
    let after = trimmed[last_idx + last_ch.len_utf8()..].trim();
    if !after.is_empty() {
        return Some(after);
    }
    let completed_start = trimmed[..last_idx]
        .char_indices()
        .rev()
        .find(|(_, ch)| SENTENCE_TERMINATORS.contains(ch))
        .map(|(idx, ch)| idx + ch.len_utf8())
        .unwrap_or(0);
    let completed = trimmed[completed_start..last_idx].trim();
    if completed.is_empty() {
        None
    } else {
        Some(completed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(text: &str) -> bool {
        let mut monitor = DegenerateLoopMonitor::default();
        // Feed in small chunks, like real stream deltas.
        let bytes = text.as_bytes();
        let mut consumed = 0;
        while consumed < bytes.len() {
            let mut end = (consumed + 7).min(bytes.len());
            while end < bytes.len() && !text.is_char_boundary(end) {
                end += 1;
            }
            if monitor.feed(&text[consumed..end]) {
                return true;
            }
            consumed = end;
        }
        false
    }

    #[test]
    fn repeated_sentence_trips() {
        // The incident shape: a SHORT sentence over and over (needs the
        // avalanche threshold, far below it here).
        let looped = "写实现。".repeat(200);
        assert!(fed(&looped), "an endless sentence loop must trip");
    }

    #[test]
    fn repeated_sentence_needs_the_threshold() {
        // Long sentence (14 chars): 5 copies trip, 4 don't.
        let few = "这一句足够长不会被阈值误伤。".repeat(TRIP_REPETITIONS - 1);
        assert!(!fed(&few), "repetition below the threshold must not trip");
        let at = "这一句足够长不会被阈值误伤。".repeat(TRIP_REPETITIONS);
        assert!(fed(&at), "repetition at the threshold trips");
    }

    #[test]
    fn short_sentence_needs_the_avalanche() {
        let below = "写实现。".repeat(SHORT_TRIP_REPETITIONS - 1);
        assert!(!fed(&below), "39 copies of a 4-char sentence is tolerable");
        let at = "写实现。".repeat(SHORT_TRIP_REPETITIONS);
        assert!(fed(&at), "40 copies inside the window is a loop");
    }

    #[test]
    fn tiny_segments_are_exempt() {
        // A 1-char tail can never reach its (infinite) bar.
        let short = "a\n".repeat(500);
        assert!(!fed(&short), "1-char segments never trip");
        // 2-char segments sit on the avalanche bar: 20 copies fine.
        let tolerable = "ok\n".repeat(20);
        assert!(!fed(&tolerable));
    }

    #[test]
    fn normal_prose_does_not_trip() {
        // Two rounds of a three-sentence paragraph: the trailing sentence
        // appears twice — well under the long-sentence bar.
        let prose = "第一段说明背景与动机，展开实现思路的取舍。第二段给出验证步骤与预期输出格式。第三段总结风险并提示后续跟进事项。".repeat(2);
        assert!(!fed(&prose), "varied prose must not trip");
    }

    #[test]
    fn window_stays_bounded() {
        let mut monitor = DegenerateLoopMonitor::default();
        let filler: String = (0..200)
            .map(|i| format!("解释第{i}段：内容各不相同，用来把窗口推过上限。"))
            .collect();
        assert!(!monitor.feed(&filler));
        assert!(monitor.tail.chars().count() <= WINDOW_CHARS);
    }

    #[test]
    fn ascii_sentences_trip_too() {
        let looped = "The static method must be called on the type. ".repeat(60);
        assert!(fed(&looped), "the detector is script-agnostic");
    }

    /// BUG-152: the second incident shape (2026-10-14 17:37, session
    /// bc6ff9a5) — the same status sentence restated with small lexical
    /// variations. No sentence repeats verbatim enough for the exact bar,
    /// yet the stream is a loop and must trip the fuzzy channel.
    #[test]
    fn paraphrase_loop_trips() {
        let looped = "正在编写实现。MCP server 再次不稳定了——这里并不需要它。\
现在正在编写实现。CHyprColor 为运行时——使用常量即可。正在编写实现代码。\
现在开始编写实现。我正在编写实现代码。所有模式均已确认。正在编写实现代码。\
现在正在编写实现。所有模式已确认。我正在编写实现代码。正在编写实现。\
现在开始编写实现。上下文已全部就绪。我正在编写实现代码。正在编写实现。";
        assert!(
            fed(looped),
            "a paraphrase loop (varied wording, same content) must trip"
        );
    }

    /// Genuinely distinct prose never accumulates near-copy families.
    #[test]
    fn distinct_stepwise_output_does_not_trip() {
        let steps = "先安装工具链再拉取仓库依赖，锁定版本号。\
接着配置编译环境变量与目标三元组。\
然后运行单元测试套件并收集覆盖率报告。\
随后执行端到端回归并归档失败截图。\
再由评审人核对提交信息与变更范围。\
最后把验收结论写回工单并关闭任务。\
整套流程结束后清理临时构建目录。\
导出本次迭代的性能基线数据。\
更新变更日志与升级注意事项。\
向值班通道同步发布窗口安排。\
复核密钥轮换与凭据扫描结果。\
收尾时确认所有看板状态一致。";
        assert!(!fed(steps), "varied real work is not a loop");
    }

    /// Below the trip bar a small family plus filler must stay quiet — the
    /// fuzzy channel needs 10 near copies, not 3.
    #[test]
    fn small_paraphrase_family_stays_below_the_bar() {
        let family = "正在编写实现。现在正在编写实现。正在编写实现代码。";
        let filler = "先读取配置文件。然后解析命令行参数。接着初始化日志。\
加载工作区设置并校验路径。最后进入主循环等待输入。";
        assert!(!fed(&format!("{family}{filler}")));
    }

    #[test]
    fn dice_coefficient_bounds() {
        let mut a: Vec<char> = "正在编写实现。".chars().collect();
        let mut b: Vec<char> = "现在正在编写实现。".chars().collect();
        a.sort_unstable();
        b.sort_unstable();
        assert!(dice_similarity(&a, &b) >= 0.75, "variants are cousins");
        let mut c: Vec<char> = "完全不同的两句话内容。".chars().collect();
        c.sort_unstable();
        assert!(
            dice_similarity(&a, &c) < 0.75,
            "unrelated sentences are not cousins"
        );
        assert_eq!(dice_similarity(&[], &a), 0.0);
    }

    #[test]
    fn incomplete_tail_is_not_a_sentence() {
        let mut monitor = DegenerateLoopMonitor::default();
        assert!(!monitor.feed("还没有结束的句子片段"));
    }
}
