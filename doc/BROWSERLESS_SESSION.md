# Browserless Session Management & Action Context

## 概述

两大能力：

1. **browserless session 显式复用**：query 里请求 `reconnect`，用 `mapper` 存下 endpoint，后续 action 在 `url` 里用 `{{ ctx.<id>.<key> }}` 引用，登录态、cookies 保持
2. **action context（task 模板）**：每个 action 的输出记录在 task 级别 context 中，后续任意 action 的字符串字段可用 `{{ctx.<id>.<key>}}` 引用

## Action id 与统一输出 map

action 条目支持 `id` 与 `mapper` 元字段：

```toml
{ id = "login", mapper = { url = "data" }, click = { key = "📱" } }
```

- `id`：action 的标识，供模板引用，**缺省为 0-based index**（第一个 action 是 `{{ctx.0.…}}`，第二个是 `{{ctx.1.…}}`……）。接受字符串或整数
- `mapper`：描述如何把该 action 的输出转换为对应的 k:v 存储（取自 `data` 值的路径），**所有 action 通用**（不限 browserless）

**每个 action 的输出都是统一的 map 形式**：

| Action | 输出 map |
| -- | -- |
| `text` / `dice` | `{ data = "发送的消息文本" }` |
| `click` | `{ data = "回调应答文本或按钮 URL" }` |
| `browserless` | `{ data = <BQL 响应 data 对象> }` |
| `llm` | 其选择的 action 的输出 map |

配置了 `mapper` 的任何 action 会在 `data` 之外追加提取的 key。

## mapper（所有 action 通用）

`mapper` 是 action 条目级配置，只表示如何把该 action 的输出转换为 k:v 存储，每条是 `key = "json_path"`（相对 `data` 的点分路径，可带可不带 `data.` 前缀）：

```toml
# browserless action: 提取 BQL 响应字段
{ id = "scrape", mapper = { code = "data.text.text", status = "data.goto.status" },
  browserless = { token = "T", query = "mutation S { text(selector: \".verify-code\") { text } }", operation_name = "S" } }

# text action 同样可用: 把发送文本改名为 msg
{ id = "hello", mapper = { msg = "data" }, text = { text = "hello" } }
```

browserless 执行后输出 map：

```json
{
  "data": { "text": { "text": "123456" }, "goto": { "status": 200 } },
  "code": "123456",
  "status": "200"
}
```

不配置 mapper 时只有 `data` 一个 key。

## task 模板语法

任何 action 的任何字符串字段（`text`、`click.key`、`llm.prompt`、`browserless.query/token/operation_name/url`，含未来新增字段）都支持在 `{{ }}` 内写路径表达式：

| 写法 | 含义 |
| -- | -- |
| `{{ctx.0.data}}` / `{{ctx.login.data}}` | 该 action 输出 map 的 `data` key |
| `{{ctx.login.code}}` | mapper 提取的 key |
| `{{ctx.0.data.goto.status}}` | 按 JSON 路径穿透 `data` 取 BQL 字段 |
| `{{ctx.0.data.items[0]}}` | 数组下标 |
| `{{ctx.3.code}}` | 用数字 id（缺省 index）引用 |

```toml
{ text = { text = "验证码是 {{ctx.scrape.code}}" } }
{ text = { text = "登录页状态: {{ctx.login_page.data.goto.status}}" } }
```

渲染规则：

- `{{ }}` 外的文本原样保留（GraphQL 的单花括号 `{ status }` 不受影响）
- 数字/布尔直接渲染为字符串；对象渲染为 JSON；null 渲染为空串
- 整个字段只有一个表达式时保留引用值的类型语义（字段本身是字符串则字符串化）

### 错误处理

- 引用不存在的 id/key：task 失败并报错，报错带路径详情
- `{{` 未闭合、表达式为空或语法错误：task 失败并报错
- 重复 id：task 失败

## Query 变量（显式）

browserless 不会隐式接收上一个 action 的结果。需要给 query 传 GraphQL 变量时，在 action 里写 `variables`，值是普通字符串字段，先渲染模板再发送；值是 Telegram miniapp 链接（`https://t.me/<bot>/<app>`）时会先解析成实际的 web view URL：

```toml
{ id = "open", click = { key = "📱" } },
{ browserless = { token = "T", operation_name = "Go", variables = { url = "{{ ctx.open.data }}" },
    query = "mutation Go($url: String!) { goto(url: $url) { status } }" } },
```

不配置 `variables` 时请求里不带 `variables` 字段。

## Session 复用（显式）

没有任何隐式 session 状态：浏览器会话的 endpoint 就是普通的 action 输出，由你在配置里串起来。

1. query 里请求 `reconnect(timeout: 30000) { browserQLEndpoint }`，响应的 `data.reconnect.browserQLEndpoint` 即会话 endpoint（`timeout` 是浏览器在查询结束后保留的毫秒数，两次 browserless 之间必须在这个窗口内）
2. 用该 action 的 `mapper` 把 endpoint 存成一个 key，如 `url = "data.reconnect.browserQLEndpoint"`
3. 后续 browserless action 把 `url` 设为 `"{{ ctx.<id>.url }}"`；`url` 和其他字符串字段一样先渲染模板，再作为请求 endpoint（token 自动附加）

```toml
actions = [
    { id = "page", mapper = { url = "data.reconnect.browserQLEndpoint" },
      browserless = { token = "T", operation_name = "Go",
        query = "mutation Go { goto(url: \"https://example.com\") { status } reconnect(timeout: 30000) { browserQLEndpoint } }" } },
    { text = { text = "/checkin" } },                       # 不影响 session
    { browserless = { token = "T", operation_name = "S", url = "{{ ctx.page.url }}",
        query = "mutation S { click(selector: \"button\") { time } }" } },   # 复用同一个浏览器
]
```

`url` 不设置时使用默认 endpoint `https://production-sfo.browserless.io/stealth/bql`。

## 完整示例

```toml
[[task]]
name = "verify_flow"
chat_id = "123456789"
actions = [
    { text = { text = "/start" } },
    { id = "open", click = { key = "📱" } },
    { id = "go", mapper = { url = "data.reconnect.browserQLEndpoint" }, browserless = { token = "T", variables = { url = "{{ ctx.open.data }}" }, query = "mutation Go($url: String!) { goto(url: $url, waitUntil: networkIdle) { status } reconnect(timeout: 30000) { browserQLEndpoint } }", operation_name = "Go" } },
    { id = "scrape", mapper = { code = "data.text.text" }, browserless = { token = "T", url = "{{ ctx.go.url }}", query = "mutation S { text(selector: \".verify-code\") { text } }", operation_name = "S" } },
    { text = { text = "验证码是 {{ctx.scrape.code}}" } },
    { text = { text = "验证码是 {{ctx.scrape.data.text.text}}" } },
    { text = { text = "验证码是 {{ctx.3.code}}" } },
]
```

## 最佳实践

- 给常用字段配 `mapper`，`{{ctx.scrape.code}}` 的可读性远好于长 JSON 路径
- 用语义化 `id`（如 `login`、`scrape`）代替裸 index

## 核心设计：所有状态都在 TaskContext

所有跨 action 状态（最近发送的消息、每个 action 的输出 map）都放在 task 级 `TaskContext`，不再有相邻 action 间的临时传递：

```text
run_task:
    context = TaskContext::default()          # 每个 task 一份
    for (index, entry) in actions:
        id = entry.id 或 index
        rendered = render_action(entry.action, &context)   # task 模板在此展开
        output = execute_action(..., &mut context)         # 消息/URL/输出都写入 context
        context.record_output(id, entry.mapper, output)    # 记录统一输出 map
```

context 内的状态：`text`/`dice` 写入 `last_message`（`Message` 无法表示为 k:v，单独存放；`click` 优先在其上找按钮）。其余状态都是同一个 state k:v map：每个 action 的输出以 action id 为 key 存入（即模板里的 `ctx.<id>`），没有任何专用 key。输出的 `data` 默认就是该 action 的结果（`click` 是回调文本或 URL 按钮链接，`browserless` 是 BQL 响应的 `data`）；配置了 `mapper` 则再转换为对应的 key，路径不存在直接报错。除 `last_message` 外没有任何隐式衔接：browserless 的 query 变量写在 `variables` 里（如 `variables = { url = "{{ ctx.open.data }}" }`），session 复用写在 `url` 里，见上文“Session 复用（显式）”。
