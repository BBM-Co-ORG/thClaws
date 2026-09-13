# บทที่ 15 — Subagents

Tool `Task` ช่วยให้ agent หลัก **มอบหมายงาน** ให้กับ sub-agent ซึ่งก็คือ
copy ของ agent ที่แยกออกมาต่างหาก โดยมี tool scope และเป้าหมาย
เป็นของตัวเอง เหมาะกับงานที่ต้องแตกสาขา (เช่น explore หลายแนวทาง
แบบขนาน) การปกป้อง context หลัก (รันการสำรวจที่มีข้อมูลรก ๆ ใน child)
หรืองานเฉพาะทาง (เช่น ส่งต่อให้ agent แบบ "reviewer" ที่ใช้ tools
แบบอ่านอย่างเดียว)

Subagents เป็นส่วนหนึ่งของ process เดียวกัน โดยรันอยู่ใน memory ไม่ได้แยก
เป็น OS process ต่างหาก หากต้องการ parallelism จริง ๆ ข้าม process
ให้ดู Agent Teams ในบทที่ 17

## หน้าตาเป็นอย่างไร

```
❯ are the REST endpoints in this repo consistent with our naming
  convention in AGENTS.md?

[tool: Task: (agent=reviewer, prompt=Check every route under src/api …)] …
  [child:reviewer] Using Glob to find route files…
  [child:reviewer] Found 14 routes; 3 don't match the convention
[tool: Task] ✓

Looking at the sub-agent's findings:
- `src/api/v1/getUsers.ts` should be `get_users.ts` per convention.
- `src/api/v1/FetchOrders.ts` should be `fetch_orders.ts`.
- `src/api/v2/createPost.ts` should be `create_post.ts`.
```

Parent จะเห็นเพียง response ที่เป็น text สุดท้ายของ sub-agent เท่านั้น
เสียงรบกวนจากการใช้ tool ระหว่างทางจึงไม่ไหลเข้ามาใน context หลัก

## การนิยาม agent

พฤติกรรมเฉพาะของ sub-agent ตั้งค่าได้ที่
`.thclaws/agents/*.md` (ระดับโปรเจกต์) หรือ `~/.config/thclaws/agents/*.md`
(ระดับผู้ใช้)

```markdown
---
name: reviewer
description: Read-only code review with focus on conventions
model: claude-haiku-4-5
tools: Read, Glob, Grep, Ls
permissionMode: auto
maxTurns: 20
color: cyan
---

You are a code reviewer. Look at the code the parent points you at.
Flag:
- Naming inconsistencies with the project's `AGENTS.md` conventions.
- Missing tests alongside new code.
- Security-sensitive patterns (raw SQL, unsanitised input).

Return a concise bullet list. Don't propose fixes unless asked.
```

ฟิลด์ใน frontmatter:

| Field | วัตถุประสงค์ |
|---|---|
| `name` | id ที่ไม่ซ้ำ (ค่าเริ่มต้นใช้ชื่อไฟล์) |
| `description` | ข้อความที่ parent จะเห็น บอกว่าควรใช้ agent นี้เมื่อไร |
| `model` | override โมเดลสำหรับ agent นี้ |
| `tools` | tool allowlist คั่นด้วย comma |
| `disallowedTools` | tool denylist |
| `permissionMode` | `auto` หรือ `ask` (เหมาะกับ agent แบบ "อ่านอย่างเดียว") |
| `maxTurns` | จำนวน iteration สูงสุด (ค่าเริ่มต้น 200) |
| `color` | สีในเทอร์มินัลสำหรับ output ของ child |
| `isolation` | `worktree` ให้ agent นี้มี git worktree ของตัวเอง (ใช้ได้เฉพาะใน teams) |

### เขียนใน GUI — `/agent new` / `/agent edit`

ใน desktop GUI ไม่ต้องแก้ `.md` เอง มี 2 คำสั่ง (GUI เท่านั้น) ที่เปิด
editor modal แยกช่อง YAML frontmatter กับ system prompt:

- **`/agent new <name>`** — เปิด starter definition สำหรับ agent ใหม่
- **`/agent edit <name>`** — เปิด agent เดิม (โหลดจาก disk หรือ
  reconstruct จาก built-in อย่าง `translator`) มาแก้

Save จะเขียนเป็น **project override** ที่ `.thclaws/agents/<name>.md`
def ใหม่/ที่แก้ spawn ผ่าน `/agent <name>` ได้ทันที ส่วน model-driven
`Task` จะเห็น session หน้า ใน CLI คำสั่งนี้แค่ชี้ path ให้ไปแก้เอง
(ไม่มี modal ในเทอร์มินัล)

## การเรียกใช้

มี **2 surface** สำหรับ spawn subagent:

### Model-driven — `Task` tool

Agent หลักเรียกผ่าน `Task`:

```
Task(agent: "reviewer", prompt: "Check src/api for naming violations")
```

โดยทั่วไปคุณไม่ต้องเรียกตรง ๆ เพียงถามคำถามกับ parent เป็นภาษาไทย/อังกฤษ แล้วตัวโมเดลจะตัดสินใจเอง เพราะจะเห็นรายการ agent ที่ใช้ได้ใน system prompt (ซึ่ง render มาจากการนิยาม agent)

`Task` tool **block parent's turn** จนกว่า child จะเสร็จ — parent เห็นผลเป็น tool result แล้วทำต่อ. การ reasoning ระหว่างทางของ child ไม่ echo เข้า context ของ parent (จุดประสงค์คือไม่ให้ main context รก) แต่ parent ต้องรอ child run จบก่อนจะทำการต่อไป

### User-driven — `/agent` slash command (GUI)

ในแท็บ Chat ของ desktop GUI คุณ spawn subagent **ด้วยตัวเอง**ได้โดยไม่ผ่าน reasoning ของ main agent:

```
/agent translator แปลไฟล์ src/foo.md เป็นภาษาไทย
```

Chat surface ยืนยัน: `✓ spawned background agent 'translator' (id: side-a1b2c3d4)` — id คือ `side-` ตามด้วย 8 หลักแรกของ UUID (hex) ขณะ translator กำลังรัน:

- **Main agent ยังรับ input ของคุณได้** — คุณคุยกับ main ต่อไปได้. Side-channel agent รันอยู่บน tokio task ของตัวเอง concurrent กับ main
- **History ของ main ไม่ถูกแตะ** — prompt + result ไม่เข้า main conversation. ความคืบหน้าแสดงใน sidebar **Background Agents** แยกต่างหาก (เขียว = กำลังทำงาน, เทา = เสร็จ, แดง = error) และเมื่อเสร็จจะมี system message บรรทัดเดียวลงใน chat (`✓ /translator done in Ns …`)
- **Cancel แยกอิสระ** — กดปุ่ม stop ของ main **ไม่** kill side-channel. ใช้ `/agent cancel <id>` เพื่อ cancel side-channel
- **Permission request แยกแยะได้** — ถ้า side-channel ขอ approval เรื่อง `Bash` ขณะที่ main ก็ทำ tool call อยู่, modal จะติดป้ายแหล่งของ request ("translator (background) wants to run Bash" vs "Main wants to run Bash") ไม่ approve ผิดตัว

```
/agents                       # list active background agents
/agent cancel side-a1b2c3d4   # signal cancel ตาม id
```

Side-channel ใช้ AgentDef registry เดียวกับ Task — agent ที่เรียกผ่าน `Task` ได้ ก็เรียกผ่าน `/agent` ได้เช่นกัน. Permissions, sandbox, MCP servers, KMS access ทำงานเหมือนกันทุกอย่าง

`/agent` คือ surface ที่เหมาะเมื่อ**คุณ**รู้ชัดว่าอยากให้ specialist ทำอะไร, งานมีขอบเขตชัด, และต้องการคุยกับ main ต่อระหว่างที่ specialist ทำงานอยู่. Model-driven `Task` ยังเป็นตัวเลือกที่ถูกต้องเมื่อให้ reasoning ของ parent agent ตัดสินใจว่าจะ delegate หรือไม่/เมื่อไหร่

## การเรียกซ้อน (Recursion)

Sub-agent สามารถ spawn sub-agent เพิ่มได้ลึกถึง `max_depth = 3` ตาม
ค่าเริ่มต้น โดยแต่ละระดับจะมีขอบเขตแคบลงไปเรื่อย ๆ

```
parent (depth 0)
 ├─ reviewer (depth 1) — "look at auth routes"
 │   └─ specialist (depth 2) — "audit JWT signing"
 └─ tester (depth 1) — "write integration tests"
```

เมื่อถึง depth 3 tool Task จะถูกปิดเพื่อป้องกันการเรียกซ้อนแบบไม่รู้จบ

## ลำดับการโหลด

Built-in (ฝังในไบนารี) → `~/.config/thclaws/agents.json` →
`~/.claude/agents/*.md` → `~/.config/thclaws/agents/*.md` →
`.claude/agents/*.md` → `.thclaws/agents/*.md`
โดยตัวหลังจะชนะเมื่อชื่อซ้ำกัน

## ติดตั้ง subagent จาก marketplace

Subagent เป็น 1 ใน 4 ประเภทของ thClaws marketplace (ดู
[บทที่ 12](ch12-skills.md)) catalog subagent แพ็กเป็นไฟล์ `.md` เดียว
(frontmatter + system prompt) ติดตั้งด้วยคำสั่งชุดเดียวกับ skill:

```
❯ /subagent marketplace            # list subagent ใน catalog
❯ /subagent search review          # ค้นหา
❯ /subagent info reviewer          # ดูรายละเอียด
❯ /subagent install reviewer       # ติดตั้งด้วยชื่อใน catalog
```

`/subagent install` รับชื่อใน catalog, git URL (พร้อม subpath
`#<branch>:<path/to/agent.md>`), หรือ `.zip` ลงไฟล์ที่
`.thclaws/agents/<name>.md` (project) เป็นค่าเริ่มต้น หรือ
`~/.config/thclaws/agents/` ด้วย `--user` ชื่อเอามาจาก frontmatter
`name:` (หรือ override: `/subagent install <url> <name>`) ส่วน entry
แบบ `linked-only` จะชี้ไป upstream repo แทนการติดตั้ง

ใน GUI แท็บ **Subagents** ของ browser `/marketplace` ทำแบบเดียวกันได้
ในคลิกเดียว subagent ที่เพิ่งติดตั้ง spawn ผ่าน `/agent <name>` ได้ทันที
ส่วน model-driven `Task` เห็น session หน้า

## Built-in subagent

thClaws มี subagent ชุดหนึ่งที่ ship มาในไบนารี — ไม่ต้องติดตั้งเพิ่ม
จะเห็นใน `Task(agent: "...")` และ `/agent <name>` ปกติ ถ้าจะ override
สร้าง `.thclaws/agents/<name>.md` ที่ชื่อเดียวกัน — ตัวบนดิสก์ชนะ

| ชื่อ | model default | หน้าที่ |
|---|---|---|
| `dream` | โมเดลของ session | Consolidate KMS ของ project โดยอ่าน session ล่าสุด, dedupe page, ดึง insight ออกมา เรียกผ่าน `/dream` (ดู [บทที่ 9](ch09-knowledge-bases-kms.md)) |
| `translator` | โมเดลของ session | แปลข้อความ/ไฟล์ระหว่างภาษา รักษา structure ของ markdown (heading, list, code block, frontmatter) เรียกผ่าน `/agent translator <prompt>` หรือ `Task(agent: "translator")` |
| `folder-indexer` | โมเดลของ session | ทำสารบัญของโฟลเดอร์เป็น `<folder>/index.md` — หนึ่งแถวต่อหนึ่งไฟล์ พร้อมคำอธิบายที่อ่านจากเนื้อหาจริง (ทั้งข้อความ เอกสาร และรูป) ทำงานแบบ incremental: tool `FolderIndex` ทำ fingerprint ทุกไฟล์ จึงอ่านซ้ำเฉพาะไฟล์ที่เปลี่ยนไปจากรอบก่อน เรียกผ่าน `/index <folder>`, คลิกขวาโฟลเดอร์ในแท็บ Files → **Index folder…** หรือ `Task(agent: "folder-indexer")` |
| `kms-linker` | โมเดลของ session | ซ่อม link หน้าที่พัง, refresh หน้าที่ stale, เติม index entry ที่ขาดใน KMS ถูก dispatch เป็น side-channel โดย `/kms wrap-up --fix` |
| `kms-reconcile` | โมเดลของ session | หาและแก้ความขัดแย้งข้ามหน้าใน KMS — rewrite หน้าที่ล้าสมัยพร้อม History section, ทำเครื่องหมายเคสกำกวมเป็น Conflict page ถูก dispatch โดย `/kms reconcile <name> [--apply]` |
| `kms-maintain` | โมเดลของ session | คำสั่งร่มสำหรับงานดูแลในครั้งเดียว — แก้โครงสร้าง กระทบยอดแหล่งที่มากับ session ที่มีอยู่จริง รีเฟรชของเก่า และแก้ข้อขัดแย้ง รวมเป็น pipeline เดียว เรียกด้วย `/kms maintain <name> [--apply]` ดู[บทที่ 9](ch09-knowledge-bases-kms.md) |
| `summarizer` | โมเดลของ session | ย่อข้อความ ไฟล์ หรือ URL ให้สั้นลงโดยคงความถูกต้อง — ประเด็นหลัก การตัดสินใจ สิ่งที่ได้เรียนรู้ และเลือกให้ย่อเป็นอีกภาษาได้ เรียกด้วย `/agent summarizer <prompt>` หรือ `Task(agent: "summarizer")` |
| `content-extractor` | โมเดลของ session | แปลงหน้าเว็บ ไฟล์ในเครื่อง หรือข้อความที่วางมา ให้เป็นบทความ markdown ที่สมบูรณ์ในตัว พร้อมดาวน์โหลดรูปมาเก็บไว้ในเครื่อง เหมาะใช้เป็นขั้นก่อน `/kms ingest` |

ทั้งสอง built-in ไม่ได้ระบุ `model:` ใน frontmatter — ทั้งคู่จะ inherit
โมเดลที่ session ใช้งานอยู่ เพื่อไม่ให้ผู้ใช้ข้าม provider เจอ error
"model not found" override รายตัวได้ผ่าน `settings.json` (ด้านล่าง)

### Override model ผ่าน `settings.json`

แต่ละ built-in subagent มี recommended model ที่ override ได้ผ่าน
`settings.json` โดยไม่ต้อง fork AgentDef ทั้งไฟล์ AgentDef.model เป็น
single string (ไม่มี priority list):

```json
// .thclaws/settings.json (project) หรือ ~/.config/thclaws/settings.json (user)
{
  "translator_subagent_model": "claude-sonnet-4-6"
}
```

ลำดับการ resolve:

1. ไฟล์บนดิสก์ `<scope>/.thclaws/agents/translator.md` — override เต็ม
   (แทนทั้ง AgentDef รวม instructions) ใช้เมื่ออยาก customize prompt body
2. ฟิลด์ใน `settings.json` (เช่น `translator_subagent_model`) —
   override เฉพาะโมเดล ไม่แตะ body ที่ฝัง
3. โมเดลที่ session ใช้งานอยู่ — fallback เมื่อไม่มี override
   (built-in ที่ฝังมาไม่มี `model:` frontmatter)

built-in subagent ตัวต่อ ๆ ไปที่ต้องการ tunability จะมีฟิลด์ของตัวเอง
(`<name>_subagent_model`) — convention เดียวกับฝั่ง skill
(`extract_save_skill_models`) — discoverability ดีกว่า map ทั่วไป

### Agent ที่มาจาก plugin

Plugins (บทที่ 16) สามารถส่ง agent def มาได้ผ่านรายการ `agents` ใน
manifest โดย directory เหล่านั้นจะถูก walk **หลังจาก** ที่อยู่มาตรฐาน
และถูก merge แบบ **เพิ่มเข้าไป** เท่านั้น agent จาก plugin ไม่สามารถ override
agent ของผู้ใช้หรือโปรเจกต์ที่ใช้ชื่อซ้ำกันได้ ซึ่งหมายความว่า

- สามารถติดตั้ง plugin ที่ส่ง `reviewer` + `tester` +
  `architect` มาได้ และทั้งสามจะพร้อมใช้งานผ่าน `Task(agent: "…")`
  รวมถึงการ spawn ภายใน team
- หากต่อมาคุณเพิ่ม `.thclaws/agents/reviewer.md` ของตัวเอง ของคุณจะชนะ
  ส่วนของ plugin จะถูกละเว้นไปจนกว่าคุณจะลบของตัวเองออก
- `/plugin show <name>` จะแสดงรายการ `agent dirs` ที่ plugin นั้นเพิ่มเข้ามา

## Subagents vs Side-channel agents vs Teams

| | Task subagent | `/agent` side-channel | Teams |
|---|---|---|---|
| **Trigger** | Model ตัดสินใจผ่าน `Task` tool | User พิมพ์ `/agent` | Model ใช้ `SpawnTeammate` |
| **โมเดล process** | อยู่ใน process เดียว, block parent's turn | อยู่ใน process เดียว, tokio task แยก, concurrent กับ parent | หลาย process ของ `thclaws --team-agent` ประสานด้วย tmux |
| **Parallelism** | Serial (recursion depth ไม่ใช่ concurrency) | Concurrent กับ main แต่ side-channel แต่ละตัว sequential | Concurrent อย่างแท้จริง |
| **History ของ main** | Tool result เข้า context ของ parent | ไม่ถูกแตะ — ผลลัพธ์เป็น side bubble แยก | ไม่ถูกแตะ — teammate มี session ของตัวเอง |
| **การแยกส่วน** | ใช้ sandbox ร่วมกัน | ใช้ sandbox ร่วมกัน | เลือกใช้ git worktree แยกต่อคนได้ |
| **Cancel** | Inherit cancel ของ parent | อิสระ — `/agent cancel <id>` | อิสระ — `kill` process ของ teammate |
| **การสื่อสาร** | ไม่มี — child คืนค่าเป็น string | ไม่มี — ผลสุดท้ายมาเป็น event | Filesystem mailbox + task queue |
| **Overhead** | น้อยมาก | น้อยมาก | สูง — ต้องเปิด process เพิ่มอย่างน้อย 1 ตัว |
| **เหมาะกับ** | ลด sub-problem ที่ model ตัดสินใจเอง | งาน user-driven ข้างเคียงระหว่าง main ทำงาน | สายงาน parallel ที่รันยาว |

กฎง่าย ๆ:

- **Default ใช้ Model-driven `Task`** — ปล่อยให้ parent agent ตัดสินใจว่าควร delegate เมื่อไหร่. Ceremony น้อยที่สุด
- **ใช้ `/agent`** เมื่อ*คุณ*รู้ชัดว่าต้องการให้ specialist ทำอะไร และต้องการคุยกับ main ต่อระหว่างที่ specialist ทำงาน. "แปลไฟล์นี้เป็นไทยระหว่างที่ฉันยังเขียน code อยู่"
- **ใช้ teams** เมื่องานแตกสาขาเป็น stream ยาว ๆ parallel จริง ๆ (build backend + frontend + ops 3 process ขนานกัน)
