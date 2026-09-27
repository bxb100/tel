This is a Telegram MTProto API CLI, which uses [grammers](https://codeberg.org/Lonami/grammers) as the client, using SQLite to store session data. The main purpose is to simulate a user interacting with Telegram.

app_id: `9911045`
api_hash: `45ae393448c97ddbd6c74d02b31ea024`

## Commands

### Login

Log in and then store the necessary data, e.g., session file path, user_id, name, etc.

### Logout

Log out and then remove stored data

### Gen

#### Task

Tasks are multiple; when this subcommand is called, the CLI should prompt the user for task details and save them to a TOML file. Multiple configurations should prompt the user to continue or not.

Tasks have multiple actions; actions can be `text`, `dice`, `click`, `llm`, or `browserless`. The program runs in the configuration order.

```
[[task]]
name = "task_name"
chat_id = ""
chat_name = "optional, fallback to `resolveUsername` when `get_dialogs` missing this chat_id"
cron = "optional, trigger by cron expression"
delay = 30

action = [
   { text = { text = "" } },
   { dice = { dice = "name str, cli prompt with emoji like: DICE= '🎲' BASKETBALL= '🏀' , DARTS= '🎯'" } },
   { click = { key = "the key to click, prefix match on inline keyboard buttons" } },
   { llm = { prompt = "user custom prompt, and the LLM(openai compatible api) call tool: task_action(action_type, options)" } },
   { browserless = { token = "browserless token", query = "BQL(GraphQL) query or mutation", operation_name = "optional", url = "optional, defaults to https://production-sfo.browserless.io/stealth/bql" } },
]
```

`click` on a URL-type inline button returns the button's URL; a following `browserless` action picks it up as `variables.url`.

#### Browserless

The `browserless` action runs a browser automation script through a [Browserless](https://browserless.io) BQL (GraphQL) endpoint. It configures `token`, `query`, and optionally `operation_name` and `url`.

The program issues the request like:

```
curl --request POST \
  --url 'https://production-sfo.browserless.io/stealth/bql?token=${token}&proxy=residential&blockConsentModals=true' \
  --header 'Content-Type: application/json' \
  --data '{"query":"...","variables": {"url": "..."},"operationName":"..."}'
```

`variables.url` is only included when the preceding `click` action opened a URL-type inline button; otherwise the request omits `variables`.

Example:

```
[[task]]
name = "web_checkin"
chat_id = ""
chat_name = "@some_bot"
action = [
   { text = { text = "/checkin" } },
   { click = { key = "签到" } },
   { browserless = { token = "TOKEN", query = "mutation zpr($url: String!) { goto(url: $url) { status } }", operation_name = "zpr" } },
]
```

### Run

- user: `optional`, stored login user's id. If not provided, and session_path is not provided as well, it will use the first stored login data; fails when there are multiple logged-in users.
- session_path: `optional`, path to an existing grammers SQLite session file. If not logged in through `tel login`, provide it with `--session-path`.
- task: `optional`, if not provided, run all tasks.
