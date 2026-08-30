# bytemaker

## 安装

```bash
cargo build --release
```

## 配置

```bash
cp .env.example .env
# 编辑 .env 填入 OPENAI_API_KEY（可选 OPENAI_MODEL / OPENAI_BASE_URL）
```

## 运行

```bash
cargo run --release
# 或
./target/release/bytemaker
```

评测子命令（第 5 章，详见 docs/5.evals.md）：

```bash
cargo run -- eval run --suite core --replay   # 离线回放盒带，CI 用（无需 OPENAI_API_KEY）
cargo run -- eval run --suite core --live     # 真实 LLM，测真实成功率
cargo run -- eval compare evals/runs/A.json evals/runs/B.json   # 回归对比
```

## 示例提示词

1. `Read the file README.md and tell me what this project is about`
2. `Create a file called test.py that prints "hello", then read it back`
3. `Find all Python files in this directory`
4. `Refactor the file hello.py: add type hints, docstrings, and a main guard` (测试 TodoWrite)
5. `List all Python files and create a summary of what each does` (测试 Hooks)