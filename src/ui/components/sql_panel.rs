// ── SQL 查询面板 ──
// 显示所有表、点击浏览数据、支持分页。

use dioxus::prelude::*;
use rusqlite::types::Value;

const PAGE_SIZE: usize = 50;

#[derive(Clone, PartialEq)]
struct TableInfo {
    name: String,
}

#[derive(Clone, PartialEq)]
struct SqlResult {
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
    total_rows: usize,
    page: usize,
    page_size: usize,
    duration_ms: u64,
}

#[component]
pub fn SqlPanel() -> Element {
    // 表列表
    let mut tables = use_signal(|| {
        load_table_list().unwrap_or_default()
    });
    let mut selected_table = use_signal(String::new);
    let mut sql_input = use_signal(String::new);

    // 查询结果
    let mut query_result = use_signal(|| Option::<SqlResult>::None);
    let mut error_msg = use_signal(|| Option::<String>::None);
    let mut running = use_signal(|| false);

    // 执行 SQL（带分页参数）
    let mut run_query = move |sql: String, page: usize| {
        if sql.is_empty() { return; }
        running.set(true);
        error_msg.set(None);
        let offset = page.saturating_sub(1) * PAGE_SIZE;
        // 先查总数
        let count_sql = format!("SELECT COUNT(*) FROM ({})", sql.trim_end_matches(';'));
        let total = (|| -> Result<usize, String> {
            let conn = crate::db::get_db().lock().map_err(|e| format!("db lock: {e}"))?;
            let mut stmt = conn.prepare(&count_sql).map_err(|e| format!("count: {e}"))?;
            let total: usize = stmt.query_row([], |row| row.get::<_, i64>(0))
                .map_err(|e| format!("count row: {e}"))? as usize;
            Ok(total)
        })().unwrap_or(0);
        // 执行分页查询
        let paged_sql = format!("{} LIMIT {} OFFSET {}", sql.trim_end_matches(';'), PAGE_SIZE, offset);
        match execute_sql(&paged_sql) {
            Ok(mut r) => {
                r.total_rows = total;
                r.page = page;
                r.page_size = PAGE_SIZE;
                query_result.set(Some(r));
            }
            Err(e) => {
                tracing::error!(target: "sql", error = %e, "sql query failed");
                error_msg.set(Some(e));
                query_result.set(None);
            }
        }
        running.set(false);
    };

    // 点击表名
    let mut select_table = {
        move |name: String| {
            let sql = format!("SELECT * FROM \"{}\"", name);
            sql_input.set(sql.clone());
            selected_table.set(name);
            run_query(sql, 1);
        }
    };

    // 翻页
    let mut go_page = {
        move |page: usize| {
            let sql = sql_input.read().clone();
            run_query(sql, page);
        }
    };

    rsx! {
        div { class: "settings-header",
            h2 { class: "settings-title", "sql" }
            span { class: "settings-subtitle", "browse and query data.db" }
        }
        div { class: "settings-section",
            // 表列表
            div { class: "sql-table-list",
                for t in tables() {
                    {
                        let name = t.name.clone();
                        let is_active = selected_table() == name;
                        rsx! {
                            button {
                                class: if is_active { "mode-pill is-active" } else { "mode-pill" },
                                onclick: {
                                    let n = name.clone();
                                    move |_| select_table(n.clone())
                                },
                                "{name}"
                            }
                        }
                    }
                }
            }

            // SQL 编辑器
            div { class: "sql-editor",
                textarea {
                    class: "sql-input",
                    placeholder: "SELECT * FROM agent_configs;",
                    value: "{sql_input}",
                    oninput: move |evt| {
                        sql_input.set(evt.value());
                        selected_table.set(String::new());
                    },
                    rows: 3,
                    spellcheck: false,
                }
                div { class: "sql-actions",
                    button {
                        class: "btn btn-send",
                        disabled: running() || sql_input.read().trim().is_empty(),
                        onclick: {
                            let sql = sql_input.read().clone();
                            move |_| run_query(sql.clone(), 1)
                        },
                        if running() { "running..." } else { "execute" }
                    }
                    button {
                        class: "btn btn-cancel",
                        onclick: move |_| {
                            sql_input.set(String::new());
                            selected_table.set(String::new());
                            query_result.set(None);
                            error_msg.set(None);
                        },
                        "clear"
                    }
                }
            }

            if let Some(ref err) = error_msg() {
                div { class: "sql-error", pre { "{err}" } }
            }

            if let Some(ref res) = query_result() {
                div { class: "sql-result",
                    div { class: "sql-meta",
                        span { "duration: {res.duration_ms}ms" }
                        if res.total_rows > 0 {
                            span { "total: {res.total_rows} rows" }
                        }
                        if !res.columns.is_empty() {
                            span { "showing: {res.rows.len()} rows" }
                        }
                    }
                    if !res.columns.is_empty() {
                        div { class: "sql-table-wrap",
                            table { class: "sql-table",
                                thead {
                                    tr {
                                        for col in &res.columns {
                                            th { "{col}" }
                                        }
                                    }
                                }
                                tbody {
                                    for row in &res.rows {
                                        tr {
                                            for cell in row {
                                                td {
                                                    class: if cell.is_empty() { "sql-cell-null" } else { "" },
                                                    if cell.is_empty() { "NULL" } else { "{cell}" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        // 分页
                        if (res.total_rows + res.page_size - 1) / res.page_size > 1 {
                            div { class: "sql-pagination",
                                button {
                                    class: "btn btn-cancel",
                                    disabled: res.page <= 1,
                                    onclick: {
                                        let p = res.page.saturating_sub(1);
                                        move |_| go_page(p)
                                    },
                                    "‹ prev"
                                }
                                span { class: "sql-page-info",
                                    "page {res.page} / {((res.total_rows + res.page_size - 1) / res.page_size)}"
                                }
                                button {
                                    class: "btn btn-cancel",
                                    disabled: res.page >= (res.total_rows + res.page_size - 1) / res.page_size,
                                    onclick: {
                                        let p = res.page + 1;
                                        move |_| go_page(p)
                                    },
                                    "next ›"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn load_table_list() -> Result<Vec<TableInfo>, String> {
    let conn = crate::db::get_db().lock().map_err(|e| format!("db lock: {e}"))?;
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
    ).map_err(|e| format!("prepare: {e}"))?;
    let rows = stmt.query_map([], |row| {
        Ok(TableInfo { name: row.get(0)? })
    }).map_err(|e| format!("query: {e}"))?;
    let mut tables = Vec::new();
    for row in rows {
        tables.push(row.map_err(|e| format!("row: {e}"))?);
    }
    Ok(tables)
}

fn execute_sql(sql: &str) -> Result<SqlResult, String> {
    let start = std::time::Instant::now();

    // 只有 SELECT/WITH/PRAGMA/EXPLAIN 才返回行
    let trimmed = sql.trim().to_uppercase();
    let is_query = trimmed.starts_with("SELECT") || trimmed.starts_with("PRAGMA")
        || trimmed.starts_with("EXPLAIN") || trimmed.starts_with("WITH")
        || trimmed.starts_with("LIMIT");

    if !is_query {
        // INSERT / UPDATE / DELETE / CREATE — 直接执行，不涉及分页
        let conn = crate::db::get_db().lock().map_err(|e| format!("db lock: {e}"))?;
        conn.execute(sql, []).map_err(|e| format!("execute: {e}"))?;
        let elapsed = start.elapsed();
        return Ok(SqlResult {
            columns: Vec::new(),
            rows: Vec::new(),
            total_rows: 0,
            page: 1,
            page_size: PAGE_SIZE,
            duration_ms: elapsed.as_millis() as u64,
        });
    }

    // 查询
    let conn = crate::db::get_db().lock().map_err(|e| format!("db lock: {e}"))?;
    let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {e}"))?;
    let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();

    let rows = {
        let mut rows = Vec::new();
        let row_iter = stmt.query_map([], |row| {
            let mut vals = Vec::new();
            for i in 0..row.as_ref().column_count() {
                let val = match row.get::<_, Value>(i) {
                    Ok(Value::Null) => String::new(),
                    Ok(Value::Integer(n)) => n.to_string(),
                    Ok(Value::Real(f)) => f.to_string(),
                    Ok(Value::Text(s)) => s,
                    Ok(Value::Blob(b)) => format!("<blob {} bytes>", b.len()),
                    Err(_) => "<error>".into(),
                };
                vals.push(val);
            }
            Ok(vals)
        }).map_err(|e| format!("query: {e}"))?;
        for row in row_iter {
            rows.push(row.map_err(|e| format!("row: {e}"))?);
        }
        rows
    };
    let elapsed = start.elapsed();
    // 没有显式 COUNT 时，rows.len() 就是总数
    let total = rows.len();
    Ok(SqlResult {
        columns,
        rows,
        total_rows: total,
        page: 1,
        page_size: PAGE_SIZE,
        duration_ms: elapsed.as_millis() as u64,
    })
}
