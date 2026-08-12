# ueberneon

一个基于 **ReAct** 循环的 AI 编程 Agent 桌面应用——在本地 IDE 中运行，可以自主完成文件编辑、代码搜索、Shell 执行、任务规划等开发工作。

<p align="center">
  <img src="https://img.shields.io/badge/Rust-2024_edition-000000?style=flat-square&logo=rust" alt="Rust">
  <img src="https://img.shields.io/badge/Dioxus-0.7-blueviolet?style=flat-square" alt="Dioxus">
  <img src="https://img.shields.io/badge/LLM-OpenAI_API-00a67e?style=flat-square" alt="OpenAI">
  <img src="https://img.shields.io/badge/license-MIT-inherit?style=flat-square" alt="License">
</p>

---

## 架构概览

```
┌──────────────┐     ┌──────────────┐     ┌──────────────┐
│   Dioxus UI  │────▶│  AgentManager│────▶│  LLM Provider │
│   (桌面 GUI)  │◀────│  (Agent 缓存) │◀────│  (OpenAI API) │
└──────────────┘     └──────┬───────┘     └──────────────┘
                            │
                   ┌────────▼────────┐
                   │   ReAct Loop    │
                   │  思考 → 行动 → 观察 │
                   └────────┬────────┘
                            │
              ┌─────────────┼─────────────┐
              ▼             ▼             ▼
        ┌─────────┐  ┌──────────┐  ┌──────────┐
        │ 权限门控  │  │  工具系统  │  │ SQLite DB │
        │ 4 级模式  │  │ 16+ 内置工具│  │  持久化存储 │
        └─────────┘  └──────────┘  └──────────┘
```

## 核心特性

### Agent 模式

| 模式 | 说明 |
|------|------|
| **Cautious** | 每次工具调用前请求用户确认 |
| **Ask** | 读操作自动通过，写操作需确认 |
| **Auto** | 自动执行，仅高风险操作询问 |
| **Unrestrained** | 完全自主执行，无需确认 |

### 内置工具

- **文件操作**：`read_file`、`write_file`、`edit_file`、`multi_edit`、`ls`
- **代码搜索**：`grep`、`glob`、`code_index`
- **Shell 执行**：`bash`、`bash_output`、`kill_shell`、`read_only_bash`
- **网络**：`web_fetch`
- **任务管理**：`create_plan`、`complete_step`、`task`

### 计划模式

Agent 可以先生成一个分步计划（PlanNode 树），经用户审批后转换为 CompletionQueue 逐步执行，确保复杂任务的可控性。

### 扫描版 PDF OCR

导入扫描版 PDF 后，应用会自动检测没有文本层的页，并调用本地 ONNX 模型（默认 PaddleOCR PP-OCRv6 多语言 small，det + cls + rec）进行 OCR：

- 把模型包（含 `manifest.json` / `det_model.onnx` / `rec_model.onnx` / `rec_dict.txt`，可选 `cls_model.onnx` / `libonnxruntime.dylib`）放到 `~/.ueberneon/page-ocr-models/<模型名>/`，或在设置 → page ocr 中选择模型目录；
- 识别结果写入 `<书目录>/ocr/<页码>.json`（词级坐标，供阅读器透明选区与复制）和 `<书目录>/pages/<页码>.md`（知识库文本）；
- 扫描页与正常文本页的选中、复制、翻译交互一致；阅读器工具栏的「本页 OCR」可强制重跑当前页。
- 模型下载脚本见 `scripts/export_paddle_ocr_onnx.py`（`UEBERNEON_PAGE_OCR_SIZE=tiny|small|medium` 可切换档位，默认 small）。

### 权限系统

组合式 Check 模式：`Deny > Ask > Allow`，支持按工具、按路径、按操作类型的细粒度权限控制。

## 技术栈

| 层级 | 技术 |
|------|------|
| 语言 | Rust (edition 2024) |
| GUI | Dioxus 0.7 (desktop) |
| LLM | async-openai (OpenAI API) |
| 数据库 | SQLite (rusqlite, bundled) |
| 异步 | Tokio (full features) |
| 序列化 | serde + serde_json + schemars |
| 过程宏 | ueberneon-macros (inventory 编译期工具注册) |

## 快速开始

### 前置要求

- [Rust](https://rustup.rs/) (stable toolchain, edition 2024)

### 构建与运行

```bash
# 克隆仓库
git clone https://github.com/AzureVolition/ueberneon
cd ueberneon

# 设置环境变量
export OPENAI_API_KEY="sk-..."

# 编译运行
cargo run
```

首次运行后，数据将存储在 `~/.ueberneon/` 目录下。

## 项目结构

```
ueberneon/
├── src/
│   ├── agent/           # Agent 核心：ReAct 循环、工具 trait、Agent 管理器
│   │   └── prompts/     # 系统提示词构建（plan / explore 模式）
│   ├── db/              # SQLite 数据库层
│   │   └── metadata/    # 各表 CRUD 操作
│   ├── tools/           # 工具系统
│   │   ├── internal/    # 内置工具实现
│   │   ├── jobs/        # 后台作业管理器
│   │   └── sandbox/     # 沙箱规范
│   ├── permission/      # 权限控制层
│   ├── ui/              # Dioxus UI 组件
│   │   └── components/  # 面板组件（chat、sidebar、plan、settings…）
│   ├── llm/             # LLM 抽象层（子 crate）
│   │   └── openai/      # OpenAI 提供商 + 流式处理
│   ├── ueberneon-macros/ # #[derive(ToolMetaImpl)] 过程宏
│   ├── model.rs         # 共享数据模型
│   ├── store.rs         # 数据持久化
│   └── settings.rs      # 应用设置
├── tokens.css           # 设计令牌（Night Foundry 暗色主题）
├── Cargo.toml           # 工作空间配置
└── reasonix.toml        # Reasonix 工具链配置
```

## 设计风格

**Night Foundry** — 暗色系霓虹主题，基于 oklch 色彩空间：

- 底色：深紫调 (`oklch(13% 0.014 265)`)
- 主强调色：霓虹青 (`oklch(72% 0.20 200)`)
- 辅助强调色：霓虹粉 (`oklch(68% 0.22 330)`)
- 排版光晕效果 + blueprint 网格线

完整的 design tokens 定义在 [`tokens.css`](./tokens.css) 中。

## License

MIT
