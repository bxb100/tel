This is a Telegram MTProto API CLI, which uses [grammers](https://codeberg.org/Lonami/grammers) as the client, using SQLite to store session data. The main purpose is to simulate a user interacting with Telegram.

app_id: `9911045`
api_hash: `45ae393448c97ddbd6c74d02b31ea024`

## Commands

### Login

Log in and then store the necessary data, e.g., session string, user_id, name, etc.

### Logout

Log out and then remove stored data

### Gen

#### Task

Tasks are multiple; when this subcommand is called, the CLI should prompt the user for task details and save them to a TOML file. Multiple configurations should prompt the user to continue or not.

Tasks have multiple actions; actions can be `text`, `dice`, `click`, or `llm`. The program runs in the configuration order.

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
   { click = { key = "the key to click" } },
   { llm = { prompt = "user custom prompt, and the LLM(openai compatible api) call tool: task_action(action_type, options)" } },
]
```

### Run

- user: `optional`, stored login user's id. If not provided, and session_string is not provided as well, it will use the first stored login data; fails when there are multiple logged-in users.
- session_string: `optional`, if not logged in, should provide it.
- task: `optional`, if not provided, run all tasks.
