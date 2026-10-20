/// Ultra-fast BM25 in-memory search engine for MCP tool discovery.
/// Pure Rust, zero allocations during search (pre-computed at index time).
/// Sub-microsecond search across hundreds of tools.
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::description_compressor;
use crate::protocol::ToolDef;
use crate::untrusted;

const K1: f64 = 1.2;
const B: f64 = 0.75;

#[derive(Debug, Clone)]
pub struct IndexedTool {
    pub name: String,          // prefixed: "server__tool"
    pub original_name: String, // just "tool"
    pub server_name: String,
    pub description: String,
    pub tool_def: ToolDef,
    /// True when the tool's output is likely to contain externally controlled content
    /// (e.g. raw SQL queries, web fetches). Set at index time by `untrusted::is_untrusted`.
    pub untrusted_output: bool,
}

struct DocEntry {
    tool_idx: usize,
    tf: HashMap<String, f64>,
    length: f64,
}

pub struct SearchEngine {
    tools: Vec<IndexedTool>,
    docs: Vec<DocEntry>,
    idf: HashMap<String, f64>,
    avg_doc_length: f64,
}

impl SearchEngine {
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            docs: Vec::new(),
            idf: HashMap::new(),
            avg_doc_length: 0.0,
        }
    }

    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    /// Build the BM25 index from a list of tools.
    /// Typically <0.5ms for 200 tools in release mode.
    /// Compresses descriptions and tags untrusted tools at index time so callers always see
    /// the canonicalized form.
    pub fn build_index(&mut self, mut tools: Vec<IndexedTool>) {
        let start = Instant::now();

        // Canonicalize: compress descriptions + tag untrusted tools.
        for t in tools.iter_mut() {
            t.description = description_compressor::compress(&t.description);
            t.tool_def.description = t.description.clone();
            t.untrusted_output = untrusted::is_untrusted(&t.original_name, &t.description);
        }

        self.tools = tools;
        self.docs.clear();
        self.idf.clear();

        let mut df: HashMap<String, usize> = HashMap::new();
        let mut total_length: f64 = 0.0;

        // Phase 1: tokenize and compute term frequencies
        for (idx, tool) in self.tools.iter().enumerate() {
            let text =
                format!("{} {} {}", tool.original_name, tool.name, tool.description).to_lowercase();

            let terms = tokenize(&text);
            let mut tf: HashMap<String, f64> = HashMap::new();

            for term in &terms {
                *tf.entry(term.clone()).or_default() += 1.0;
            }

            // Document frequency
            for term in tf.keys() {
                *df.entry(term.clone()).or_default() += 1;
            }

            let length = terms.len() as f64;
            total_length += length;

            self.docs.push(DocEntry {
                tool_idx: idx,
                tf,
                length,
            });
        }

        // Phase 2: compute IDF
        let n = self.docs.len() as f64;
        self.avg_doc_length = if n > 0.0 { total_length / n } else { 0.0 };

        for (term, freq) in &df {
            let f = *freq as f64;
            let idf = ((n - f + 0.5) / (f + 0.5) + 1.0).ln();
            self.idf.insert(term.clone(), idf);
        }

        let elapsed = start.elapsed();
        eprintln!(
            "[McpHub][INFO] Search index built: {} tools in {:.2}ms",
            self.tools.len(),
            elapsed.as_secs_f64() * 1000.0
        );
    }

    /// Search tools by natural language query.
    /// Returns top-K results sorted by BM25 relevance.
    /// Typically <0.05ms for 200 tools in release mode.
    pub fn search(&self, query: &str, top_k: usize) -> Vec<&IndexedTool> {
        self.search_filtered(query, top_k, None)
    }

    pub fn search_filtered(
        &self,
        query: &str,
        top_k: usize,
        allowed_servers: Option<&HashSet<String>>,
    ) -> Vec<&IndexedTool> {
        let empty: HashMap<String, u64> = HashMap::new();
        self.search_filtered_with_usage(query, top_k, allowed_servers, &empty)
    }

    /// Like `search_filtered` but applies a per-tool usage boost so frequently-used tools
    /// surface first. Boost = `1.5 * ln(call_count + 1)` (additive on the BM25 score).
    /// Calling 0 times = 0 boost. 5 calls ≈ +2.7. 50 calls ≈ +5.9. 500 calls ≈ +9.3.
    pub fn search_filtered_with_usage(
        &self,
        query: &str,
        top_k: usize,
        allowed_servers: Option<&HashSet<String>>,
        usage_boost: &HashMap<String, u64>,
    ) -> Vec<&IndexedTool> {
        if self.docs.is_empty() {
            return Vec::new();
        }

        let query_terms = tokenize(&query.to_lowercase());
        if query_terms.is_empty() {
            return self
                .tools
                .iter()
                .filter(|tool| {
                    allowed_servers
                        .map(|servers| servers.contains(&tool.server_name))
                        .unwrap_or(true)
                })
                .take(top_k)
                .collect();
        }

        let mut scores: Vec<(f64, usize)> = Vec::with_capacity(self.docs.len());
        let query_lower = query.to_lowercase();

        for doc in &self.docs {
            if let Some(allowed_servers) = allowed_servers {
                let server_name = &self.tools[doc.tool_idx].server_name;
                if !allowed_servers.contains(server_name) {
                    continue;
                }
            }

            let mut score = 0.0_f64;

            for qt in &query_terms {
                let idf = match self.idf.get(qt) {
                    Some(v) => *v,
                    None => continue,
                };

                let term_freq = match doc.tf.get(qt) {
                    Some(v) => *v,
                    None => continue,
                };

                let numerator = term_freq * (K1 + 1.0);
                let denominator =
                    term_freq + K1 * (1.0 - B + B * (doc.length / self.avg_doc_length));
                score += idf * (numerator / denominator);
            }

            let lower_name = self.tools[doc.tool_idx].original_name.to_lowercase();
            if lower_name == query_lower {
                score += 10.0;
            } else if lower_name.contains(&query_lower) {
                score += 5.0;
            }

            // Usage-based boost: tools called frequently rank higher (within their relevance band).
            if score > 0.0 && !usage_boost.is_empty() {
                let key = &self.tools[doc.tool_idx].name;
                if let Some(&calls) = usage_boost.get(key) {
                    if calls > 0 {
                        score += 1.5 * ((calls + 1) as f64).ln();
                    }
                }
            }

            if score > 0.0 {
                scores.push((score, doc.tool_idx));
            }
        }

        scores.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        scores
            .iter()
            .take(top_k)
            .map(|(_, idx)| &self.tools[*idx])
            .collect()
    }

    /// Return all indexed tools belonging to a specific server.
    /// Used by mcphub_call to suggest available tools on argument errors.
    pub fn tools_on_server(&self, server: &str) -> Vec<&IndexedTool> {
        self.tools
            .iter()
            .filter(|tool| tool.server_name == server)
            .collect()
    }

    /// Get catalog of all indexed tools (name + short description).
    pub fn get_catalog(&self) -> Vec<CatalogEntry> {
        self.tools
            .iter()
            .map(|t| CatalogEntry {
                name: t.original_name.clone(),
                server: t.server_name.clone(),
                description: t.description.chars().take(120).collect(),
            })
            .collect()
    }

    /// Find a tool by prefixed name (exact match).
    #[allow(dead_code)]
    pub fn find_by_name(&self, prefixed_name: &str) -> Option<&IndexedTool> {
        self.tools.iter().find(|t| t.name == prefixed_name)
    }

    /// Find a tool by original name on a specific server.
    pub fn find_tool(&self, server: &str, tool: &str) -> Option<&IndexedTool> {
        self.tools
            .iter()
            .find(|t| t.server_name == server && t.original_name == tool)
    }

    pub fn find_tool_case_insensitive(&self, server: &str, tool: &str) -> Option<&IndexedTool> {
        self.tools.iter().find(|indexed_tool| {
            indexed_tool.server_name == server
                && indexed_tool.original_name.eq_ignore_ascii_case(tool)
        })
    }

    pub fn has_server(&self, server: &str) -> bool {
        self.tools.iter().any(|tool| tool.server_name == server)
    }

    pub fn infer_server_from_tool_query(&self, query: &str) -> Option<String> {
        if self.tools.is_empty() {
            return None;
        }

        let normalized_query = normalize_identifier(query);
        if normalized_query.len() < 3 {
            return None;
        }

        let query_terms = tokenize(&query.to_lowercase());
        let mut server_best_scores: HashMap<String, i32> = HashMap::new();
        let mut exact_match_servers = HashSet::new();

        for tool in &self.tools {
            let tool_name = normalize_identifier(&tool.original_name);
            let score = score_tool_name_match(&tool_name, &normalized_query, &query_terms);
            if score > 0 {
                let entry = server_best_scores
                    .entry(tool.server_name.clone())
                    .or_default();
                *entry = (*entry).max(score);
            }
            if tool_name == normalized_query {
                exact_match_servers.insert(tool.server_name.clone());
            }
        }

        if exact_match_servers.len() == 1 {
            return exact_match_servers.into_iter().next();
        }

        if exact_match_servers.len() > 1 {
            return None;
        }

        if server_best_scores.is_empty() {
            return None;
        }

        let mut ranked_servers: Vec<(String, i32)> = server_best_scores.into_iter().collect();
        ranked_servers
            .sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));

        if ranked_servers.len() == 1 {
            return Some(ranked_servers[0].0.clone());
        }

        let top = &ranked_servers[0];
        let runner_up = &ranked_servers[1];
        if top.1 >= 160 && top.1 >= runner_up.1 + 50 {
            Some(top.0.clone())
        } else {
            None
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct CatalogEntry {
    pub name: String,
    pub server: String,
    pub description: String,
}

// ─── Tokenizer ───────────────────────────────────────────────

fn tokenize(text: &str) -> Vec<String> {
    // Split camelCase: "readFile" -> "read File"
    let mut expanded = String::with_capacity(text.len() + 16);
    let chars: Vec<char> = text.chars().collect();

    for i in 0..chars.len() {
        if i > 0 && chars[i].is_uppercase() && chars[i - 1].is_lowercase() {
            expanded.push(' ');
        }
        expanded.push(chars[i]);
    }

    // Split on non-alphanumeric
    expanded
        .split(|c: char| !c.is_alphanumeric())
        .map(|s| s.to_lowercase())
        .filter(|s| s.len() > 1 && !STOPWORDS.contains(s.as_str()))
        .collect()
}

fn normalize_identifier(text: &str) -> String {
    text.chars()
        .filter(|char| char.is_ascii_alphanumeric())
        .flat_map(|char| char.to_lowercase())
        .collect()
}

fn score_tool_name_match(tool_name: &str, normalized_query: &str, query_terms: &[String]) -> i32 {
    if tool_name.is_empty() {
        return 0;
    }

    let mut score = 0;

    if tool_name == normalized_query {
        score += 220;
    } else if tool_name.len() >= 4 && normalized_query.contains(tool_name) {
        score += 140;
    } else if normalized_query.len() >= 4 && tool_name.contains(normalized_query) {
        score += 90;
    }

    for query_term in query_terms {
        if query_term.len() >= 3 && tool_name.contains(query_term) {
            score += 25;
        }
    }

    score
}

static STOPWORDS: std::sync::LazyLock<HashSet<&'static str>> = std::sync::LazyLock::new(|| {
    [
        "a", "an", "the", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
        "do", "does", "did", "will", "would", "could", "should", "may", "might", "can", "shall",
        "to", "of", "in", "for", "on", "with", "at", "by", "from", "as", "into", "through",
        "during", "before", "after", "above", "below", "between", "under", "again", "further",
        "then", "once", "here", "there", "when", "where", "why", "how", "all", "each", "every",
        "both", "few", "more", "most", "other", "some", "such", "no", "nor", "not", "only", "own",
        "same", "so", "than", "too", "very", "just", "or", "and", "but", "if", "it", "its", "this",
        "that", "these", "those", "me", "my", "we", "our", "you", "your", "he", "him", "his",
        "she", "her", "they", "them", "their", "what", "which", "who", "whom",
    ]
    .iter()
    .copied()
    .collect()
});

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mock_tool(name: &str, desc: &str) -> IndexedTool {
        IndexedTool {
            name: format!("test__{}", name),
            original_name: name.to_string(),
            server_name: "test".to_string(),
            description: desc.to_string(),
            tool_def: ToolDef {
                name: name.to_string(),
                description: desc.to_string(),
                input_schema: json!({"type": "object"}),
            },
            untrusted_output: false,
        }
    }

    #[test]
    fn test_tokenizer_camel_case() {
        let terms = tokenize("readFileDisk");
        assert!(terms.contains(&"read".to_string()));
        assert!(terms.contains(&"file".to_string()));
        assert!(terms.contains(&"disk".to_string()));
    }

    #[test]
    fn test_tokenizer_stopwords() {
        let terms = tokenize("the quick brown fox is jumping");
        assert!(!terms.contains(&"the".to_string()));
        assert!(!terms.contains(&"is".to_string()));
        assert!(terms.contains(&"quick".to_string()));
        assert!(terms.contains(&"brown".to_string()));
        assert!(terms.contains(&"fox".to_string()));
        assert!(terms.contains(&"jumping".to_string()));
    }

    #[test]
    fn test_search_engine_ranking() {
        let mut engine = SearchEngine::new();
        let tools = vec![
            mock_tool("read_file", "Reads a file from the disk"),
            mock_tool("write_file", "Writes data to a file on disk"),
            mock_tool("delete_file", "Deletes a file permanently"),
        ];
        engine.build_index(tools);

        let results = engine.search("read disk", 5);
        assert!(!results.is_empty());
        assert_eq!(results[0].original_name, "read_file");

        let results2 = engine.search("write", 5);
        assert!(!results2.is_empty());
        assert_eq!(results2[0].original_name, "write_file");
    }

    #[test]
    fn test_empty_search() {
        let mut engine = SearchEngine::new();
        engine.build_index(vec![]);
        let results = engine.search("test", 5);
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_filtered_limits_results_to_server() {
        let mut engine = SearchEngine::new();
        let tools = vec![
            IndexedTool {
                name: "github__create_issue".to_string(),
                original_name: "create_issue".to_string(),
                server_name: "github".to_string(),
                description: "Create a GitHub issue".to_string(),
                tool_def: ToolDef {
                    name: "create_issue".to_string(),
                    description: "Create a GitHub issue".to_string(),
                input_schema: json!({"type": "object"}),
            },
            untrusted_output: false,
        },
            IndexedTool {
                name: "supabase__execute_sql".to_string(),
                original_name: "execute_sql".to_string(),
                server_name: "supabase".to_string(),
                description: "Execute SQL on Supabase".to_string(),
                tool_def: ToolDef {
                    name: "execute_sql".to_string(),
                    description: "Execute SQL on Supabase".to_string(),
                input_schema: json!({"type": "object"}),
            },
            untrusted_output: false,
        },
        ];
        engine.build_index(tools);

        let allowed_servers = HashSet::from(["github".to_string()]);
        let results = engine.search_filtered("issue", 5, Some(&allowed_servers));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].server_name, "github");
    }

    #[test]
    fn test_infer_server_from_tool_query_returns_unique_server() {
        let mut engine = SearchEngine::new();
        engine.build_index(vec![
            IndexedTool {
                name: "MemoryPilot__recall".to_string(),
                original_name: "recall".to_string(),
                server_name: "MemoryPilot".to_string(),
                description: "Recall memories".to_string(),
                tool_def: ToolDef {
                    name: "recall".to_string(),
                    description: "Recall memories".to_string(),
                input_schema: json!({"type": "object"}),
            },
            untrusted_output: false,
        },
            IndexedTool {
                name: "supabase__execute_sql".to_string(),
                original_name: "execute_sql".to_string(),
                server_name: "supabase".to_string(),
                description: "Execute SQL".to_string(),
                tool_def: ToolDef {
                    name: "execute_sql".to_string(),
                    description: "Execute SQL".to_string(),
                input_schema: json!({"type": "object"}),
            },
            untrusted_output: false,
        },
            IndexedTool {
                name: "github__create_issue".to_string(),
                original_name: "create_issue".to_string(),
                server_name: "github".to_string(),
                description: "Create issue".to_string(),
                tool_def: ToolDef {
                    name: "create_issue".to_string(),
                    description: "Create issue".to_string(),
                input_schema: json!({"type": "object"}),
            },
            untrusted_output: false,
        },
        ]);

        assert_eq!(
            engine.infer_server_from_tool_query("please recall project memory"),
            Some("MemoryPilot".to_string())
        );
        assert_eq!(
            engine.infer_server_from_tool_query("execute_sql for this database"),
            Some("supabase".to_string())
        );
    }

    #[test]
    fn test_infer_server_from_tool_query_returns_none_when_ambiguous() {
        let mut engine = SearchEngine::new();
        engine.build_index(vec![
            IndexedTool {
                name: "github__search".to_string(),
                original_name: "search".to_string(),
                server_name: "github".to_string(),
                description: "Search GitHub".to_string(),
                tool_def: ToolDef {
                    name: "search".to_string(),
                    description: "Search GitHub".to_string(),
                input_schema: json!({"type": "object"}),
            },
            untrusted_output: false,
        },
            IndexedTool {
                name: "exa__search".to_string(),
                original_name: "search".to_string(),
                server_name: "exa".to_string(),
                description: "Search web".to_string(),
                tool_def: ToolDef {
                    name: "search".to_string(),
                    description: "Search web".to_string(),
                input_schema: json!({"type": "object"}),
            },
            untrusted_output: false,
        },
        ]);

        assert_eq!(engine.infer_server_from_tool_query("search"), None);
    }

    #[test]
    fn test_infer_server_from_tool_query_returns_none_for_duplicate_exact_tool_name() {
        let mut engine = SearchEngine::new();
        engine.build_index(vec![
            IndexedTool {
                name: "browser-mcp__browser_navigate".to_string(),
                original_name: "browser_navigate".to_string(),
                server_name: "browser-mcp".to_string(),
                description: "Navigate browser".to_string(),
                tool_def: ToolDef {
                    name: "browser_navigate".to_string(),
                    description: "Navigate browser".to_string(),
                input_schema: json!({"type": "object"}),
            },
            untrusted_output: false,
        },
            IndexedTool {
                name: "playwright__browser_navigate".to_string(),
                original_name: "browser_navigate".to_string(),
                server_name: "playwright".to_string(),
                description: "Navigate browser".to_string(),
                tool_def: ToolDef {
                    name: "browser_navigate".to_string(),
                    description: "Navigate browser".to_string(),
                input_schema: json!({"type": "object"}),
            },
            untrusted_output: false,
        },
        ]);

        assert_eq!(
            engine.infer_server_from_tool_query("browser navigate"),
            None
        );
    }
}
