<!-- 编辑注：本文件为记忆插件设计文档（用户提供，2026 版）。原文已去掉原框架相关内容。
     落地形态见 PLAN.md 的 B 阶段精神：记忆内核在 crates/dsh-memory，接线在 plugins/plugin-memory。 -->

# 记忆插件设计 —— 关系层记忆

## 一、定位

从"扁平事实库"升级为"认识用户、也认识自己的关系层记忆"。核心诉求：

- **陪伴感底线**：核心身份永不遗忘，琐事快速淡出
- **时间感**：能说出"两周前我们聊过 X"、"聊过 3 次"、"什么时候开始的"
- **一致性**：记得自己说过什么、答应过什么，不重复建议、不重复提问
- **可进化**：同一件事多次聊，状态合并演进，而不是堆成互相矛盾的多行
- **自我**：助手对自己的认知——我是谁（模型）、我做过什么（行为史）、我知道什么缺口（元认知）
- **自然成长**：不预设人格，身份在与用户的互动中自然形成（空开始）

## 二、架构全景图

### 2.1 总览

```
┌────────────────────────── 写路径（记忆流入） ──────────────────────────┐
│                                                                        │
│  对话 turn pair ──► 提取(字面事实) ──► 编号消解 ──► 状态合并 ──► 存储     │
│  (user+assistant)    三类事实           resolve_topic   llm_merge        │
│                      ↓用户/自己/关系   阻塞+仲裁        extend/correct/new │
│  会话转录 ──► 统计 + 推断 ────────────────┘  ← 模式/关系/缺口（人格在此） │
│                                                                        │
│  存储：关系卡(profile+agent_model) · 账本(topics) · events · promises    │
│        · self_history                                                   │
└────────────────────────────────────────────────────────────────────────┘
                                   │
┌────────────────────────── 读路径（记忆流出） ──────────────────────────┐
│                                                                        │
│  关系卡常驻注入（你是谁+我是谁+我们之间）—— 永不检索                    │
│  Mode B 时间检索（最近经历/未完成承诺/今天事件）→ 主动性燃料              │
│  Mode A 话题检索（resolve_topic → 规范行 → 排序 → top-k）                │
│  ▲ 冲突信号：检索结果 vs 关系卡矛盾 → 触发卡 diff 更新                   │
└────────────────────────────────────────────────────────────────────────┘
```

### 2.2 记忆生命周期

```
诞生   快提取从 turn 抽出陈述（action 三态）／慢消化从会话归纳模式
身份   编号消解：阻塞+仲裁 → 归并已有 id 或新建（aliases 累积表述）
演化   llm_merge：状态随每次相关对话合并演进
被唤   recall 命中 → activation+1、时间刷新、权重恢复
衰减   未提及 → 按 tier 遗忘曲线衰减（episodic 0.05/天，trivia 0.15/天）
复活   用户"还记得 X 吗" → 激活权重提升，救活将忘记忆
晋升   反复确认的稳定事实 → 被关系卡 diff 吸收（用户侧/自我侧/关系侧）
遗忘   weight < FORGET_THRESHOLD → 删除（系统衰减的活，agent 不亲手删）
```

### 2.3 机制喂给关系

| 机制 | 输入 | 输出 |
|------|------|------|
| 提取 | 当前关系卡 + 本轮 turn pair | 字面事实 diff（新增/修正/无变化） |
| 统计 | events 时间戳 | 频率/间隔/时段活跃/情绪均值（确定性） |
| 推断 | 统计 + events + 关系卡 | 模式/关系/演化/认知缺口 |
| 策略 | 规则 + 推断输出 | 晋升/遗忘/消解/淡忘决策 |
| 实体消解 | 陈述 topic_text + aliases + embeddings | topic_id（Merge/Create） |
| 状态合并 | id + 旧 state + 新陈述 | 合并后 state |
| 冲突信号 | 检索结果 vs 关系卡 | 触发卡 diff（写路径） |
| 唤醒 | recall 命中 | activation 提升 + 时间刷新 |
| Mode B | 时间 + 状态 | 最近经历/承诺/今天事件（主动性燃料） |

## 四、分层投递（双通道）

**tier 决定的是记忆"怎么到达 agent"，不是"排序维度"。**

```
关系卡（profile + agent_model）→ 常驻通道：ContextSource 固定注入，不设 token 上限，永不检索
episodic / trivia → recall 通道：按需检索（我们之间发生过什么）
```

- 关系卡是 LLM 维护的"我们俩"文档（你是谁 + 我是谁 + 我们之间），自带裁剪。identity 并入 profile（身份事实 = profile 里最稳定的部分）
- recall 默认只搜 episodic + trivia。**你不需要"查"用户是谁——它是被送到面前的**

## 五、存储 schema：双层模型 + 关系卡

```
events 表（append-only，真相源）—— 陈述原文，永不修改
  event_id | episode_id | topic_id | statement | ts

topics 表（可合并，每主题一行）—— 当前状态 + 时间锚点，recall 只查这层
  topic_id TEXT PRIMARY KEY      -- 不透明稳定 id（身份，绝不随改名变化）
  canonical_name TEXT            -- 当前规范名（标签，可随合并更新）
  aliases TEXT                   -- 历史表述集合（JSON array，参与词法阻塞）
  state_summary TEXT             -- 合并后的当前状态
  embedding BLOB                 -- embed(canonical_name + aliases + state_summary)
  created_at / last_discussed_at / n_times / tier

relation_card 表（单行，常驻注入）—— 初始为空文档，对话喂大
  profile      -- 关于用户（名字/职业/偏好/习惯/关系/情绪基线）
  agent_model  -- 关于自己（身份/行为准则/能力/关系/认知缺口）
  relationship -- 我们之间（禁区/了解程度/相处模式）
  updated_at

promises 表（关系状态）
  promise_id | topic_id | content | status(open/done/expired) | created_at | due_at?

self_history 表（行为史，按需检索）
  action_id | kind(advice/trial/failure/self-fact) | topic_id? | content | ts
```

### 分层衰减参数

| tier | 含义 | DECAY_RATE | FORGET_THRESHOLD | 投递 |
|------|------|-----------|------------------|------|
| profile | 你是谁（含 identity） | 不适用（常驻注入，diff 维护） | 不适用 | 常驻 |
| episodic | 共同经历 | 0.05 | 0.02 | recall |
| trivia | 一次性琐事 | 0.15 | 0.1 | recall（低权重） |

`activation_count` 强化为**唤醒机制**：用户主动提起 → 权重恢复甚至提升。

## 六、身份与实体消解（核心机制）

**人话版**：编号消解就是回答一个问题——"**这句新话要记到哪一行？**"找到老行就更新老行，找不到就开新行。

### 6.1 关键认知

**LLM 生成的 topic 只是"表述"，不是键。** "语义相似" ≠ "同一件事"：吉他和尤克里里语义极近但必须分两个主题；"吉他练习"和"吉他"表述不同但必须是一个。embedding 阈值永远分不清这两对。所以：

- **身份 = 系统分配的不透明 `topic_id`**，独立于表述
- **canonical_name 只是标签**，随合并改名，id 永不变
- **aliases 累积历史表述**，吸收 LLM 的不稳定性

### 6.2 两层实体消解（resolve_topic）

`resolve_topic(text) -> topic_id`：写路径 upsert 和读路径 recall **共用同一个解析器**（解析器对称性）。

```
Stage 1 — 阻塞 blocking（便宜，召回优先，零 LLM）
  candidates = embedding_topk(text, k=5)              // 语义近邻
             ∪ lexical_match(text, aliases)           // 历史表述词法命中直通
Stage 2 — LLM 仲裁 arbitration（精确，一次小调用，可并进提取调用）
  把候选和整句话丢给 LLM："这句话说的是编号7那件事吗？" → 是归并，否新建
```

只有 LLM 能看到陈述的**上下文**——"电吉他音箱"能区分"吉他"和"尤克里里"，也能把"吉他练习"并进"吉他"。

### 6.3 偏置原则（最重要的决策规则）

```
LLM 不确定时 → 保守新建（标记 uncertain）
```

- **假阳性（错误合并）不可恢复**：两件事并成一个，记忆互相污染，拆不开
- **假阴性（漏合并）可恢复**：多一行冗余，下次同一件事出现时别名词法命中把新表述引到旧行，**迟到的合并**自然发生

**宁可晚合并，不可错合并。**

## 七、记忆构造管线：异构机制，不是单一提取器

### 7.1 原则

**提取器 = 感知层（从文本抄录字面事实），不是记忆系统。** 复杂的记忆构造（聚合、模式、关系、演化、缺口）无法靠提取——单轮文本上做统计和推断只会输出噪音（幻觉）。每个机制只做一件小且可验证的事，组合起来才可信；把全部工作都塞给提取器是最危险的设计。

### 7.2 五种机制

| 机制 | 机制类型 | 触发 | 输入 → 输出 | 可信度 |
|------|---------|------|------------|--------|
| 提取 extraction | LLM | 每轮 | 关系卡 + turn pair → 字面事实 diff（三类事实：用户/自己/关系） | 有损耗，保守偏置 |
| 统计 statistics | 确定性 | 持续/写时 | events → 频率/间隔/时段活跃/情绪均值 | 精确，地面真值 |
| 推断 inference | LLM | 周期性（会话末） | 统计 + events + 关系卡 → 模式/关系/演化/认知缺口 | 高门槛，保守偏置 |
| 结构 structuring | 推断副产品 | 随推断 | 实体关系边（"Lily是同事"） | 跟随推断 |
| 策略 policy | 规则+LLM | 事件驱动 | 衰减/晋升/淡忘/遗忘/消解决策 | 规则确定性，LLM 判断保守 |

### 7.3 哪些记忆提取器永远做不出来

```
"最近两周很忙"        ← 统计（加班/截止词频）+ 推断
"Lily 是同事"         ← 关系构造（"我同事Lily"多轮出现）
"报喜不报忧"          ← 多轮情绪模式 → 推断
"不知道他生日"        ← 缺口推断：对照模板坐标系
"吉他聊过 4 次"       ← 统计（事件计数）
"被纠正了 3 次啰嗦"   ← 统计 + 推断 → agent_model 行为准则
```

### 7.4 分层质量保证

```
统计层  确定性计算 —— 精确、免费、永远可信（系统的"地面真值"）
推断层  LLM，输入是统计+事件（有据可依），不是单轮文本 —— 高门槛 + 保守偏置（不确定不写）
提取层  窄而弱，只抄字面事实 —— 错了由 correct 兜底
策略层  规则（衰减/晋升阈值）确定性；LLM 判断（仲裁/晋升确认）保守
```

**每个机制的工作越小越可验证，组合起来才可信。** 之前的"慢路径"其实就是推断层——但它是不同机制，不是"慢速提取"。

### 7.5 模板坐标系（不违背空开始）

推断层找"认知缺口"需要对照坐标系："关系卡通常有哪些维度"（名字/生日/工作/家人/偏好/作息…）。**模板是推断的坐标系，不是身份的种子**——它告诉推断层"该往哪些维度找"，不告诉它"你是谁"。

### 7.6 三类事实（提取层的产出分类）

```
关于用户   "他叫小明，喜欢晚上写代码"            → profile
关于自己   "用户给我起了名字叫小助" / "他嫌我啰嗦" → agent_model
关于关系   "他喜欢轻松的玩笑" / "他讨厌重复"       → relationship
```

## 八、写路径（apply）

每轮一次，批进提取调用：

```rust
fn apply(turn) {
    let diff = fast_extract(relation_card, turn);   // 关系卡 diff（三类事实）
    apply_card_diff(diff);                           // 卡 diff 先行
    for o in extractor.extract(turn) {               // 陈述（事件类）
        match resolve_topic(o.statement) {
            Merge(id) => {
                topics[id].state_summary = llm_merge(topics[id].state, o.statement, o.action);
                topics[id].aliases.insert(o.topic_text);
                topics[id].last_discussed_at = now;  n_times += 1;
            }
            Create => new_topic(o.topic_text, o.statement),
        }
        events.append(topic_id, o.statement);
    }
}

fn digest(session_transcript) {   // 会话末
    let diff = slow_digest(relation_card, compress(transcript));
    apply_card_diff(diff);
}
```

`action: correct`（用户"不/其实/纠正一下"）→ 替换旧状态 + 旧陈述 superseded。

## 九、读路径（recall）

### 9.1 两种模式

```
Mode A — 话题检索（有查询）
  query = agent 主动给的 query | 最近窗口实质消息（最后一条实质用户消息 + 上一条助手消息）
  填充词（嗯/好/继续/…）命中 → 降级 Mode B（stopword 表，不用 LLM）
Mode B — 时间检索（无查询 / discovery）
  不做文本匹配，按时间显著性出料：最近 N 天 episode、今天的事件、未完成承诺、情绪趋势
  ← 将来主动性层的燃料
```

### 9.2 匹配算法

```
score = semantic_sim(q, mem) × activation_factor × recency_factor
  semantic_sim   embedding 余弦（本地 Ollama，零成本）；无 embedding 退回关键词
  activation     activation_count：被提过的记忆保持温热
  recency        最近创建的 episodic 更容易相关
```

返回结构化：`{ topic, state, when(人性化"两周前"), n_times, confidence }`。

规模注记：个人陪伴记忆几千条量级，**暴力余弦在 Rust 里微秒-毫秒级，不需要向量数据库**。词法层保精确（PR 号/人名/URL），语义层保泛化，可选 RRF 合并。

### 9.3 诚实出口

低于阈值 → 显式"无相关记忆"，agent 说"这个我不记得了，是新话题吗"——**会说不记得的陪伴比会编的更有信任**。

## 十、Agent 自主记忆管理（工具面）

### 10.1 观念转变

agent 是记忆的自主管理者，用户是辅助者。**管理记忆 = 加强/减弱/检索/盘点，不是增删查改**：

```
增 → 加强（remember / 反复提及）   —— 行变强
删 → 减弱（demote）→ 系统衰减淘汰  —— 行变弱直到消失，中途可反悔
查 → recall / inventory            —— 取用 / 盘点
改 → 管线合并（extend/correct）    —— 不是 agent 改行，是自动合并
```

**agent 不亲手删任何东西**——只加强、只减弱，真正的删除是遗忘曲线的活。因为"减弱→衰减→删除"全程可逆（用户一提就复活），而"删掉"不可逆。与偏置原则一脉相承：**agent 的一切动作都偏向可恢复。**

### 10.2 工具面（4 个）

```
remember { content, topic? }    记 —— 管线漏了/用户明确说"记住这个"时加强或新建
recall   { query, limit? }      查 —— 对话中取用，返回 topic/state/when/confidence 结构化
inventory{ query?, limit? }     盘点 —— 我关于 X 知道什么 / 我整体知道什么（替代 get_memories 的 agent 版）
demote   { query, reason? }     淡忘 —— 明确减弱：标记不重要 → 权重压低位 → 快速衰减（可逆！）
```

- **inventory**：给 agent 自己盘点——"我对他知道得够不够？哪块是空白？"决定要不要主动追问
- **demote**：agent 判断"这事对他不重要了/他不想再提了" → 减弱。硬删 forget 降级为将来界面层 RPC + 用户确认，agent 不自主调

### 10.3 管理职责三层

```
自动管线（系统，日常 90%）  提取 → 归并 → 合并 → 衰减 → 晋升   全部自动，agent 不干预
Agent 主动（工具，例外）    管线漏了→remember  要背景→recall  要评估→inventory  该淡出→demote
用户辅助（信号，不是命令）  对话内容（主源）→ 管线吸收；显式指令（"记住/忘了/其实…"）→ agent 处理后落动作
```

### 10.4 用户的话怎么变成 agent 的动作

| 用户说 | agent 动作 | 机制 |
|--------|-----------|------|
| "帮我记住，我不吃香菜" | `remember{content:"不吃香菜", topic:"饮食"}` | 管线归并 |
| "之前那个吉他计划算了吧" | `demote{query:"吉他"}` | 权重压低 → 衰减淘汰 |
| "其实我不用 Rust 了" | 无需动作 | 管线 correct 自动替换 |
| （什么都没说，聊了一轮） | 无需动作 | 快提取自动吸收 |

**agent 是管理者，用户是辅助者，系统管线是执行者。**

## 十一、Agent 自我认知层

记忆不是单边的——补上"关于助手自己"：

```
agent_model（我是谁）    —— 常驻注入，镜像 profile，初始为空
  ┌ 身份: 名字/角色/说话风格
  ├ 行为准则: 一致性锚点（口癖/立场）、边界（不编造记忆、可说不知道）
  ├ 能力: 工具清单摘要、已知局限、已知失败模式
  ├ 关系: 了解程度、禁区、相处模式
  └ 认知缺口: 想补全的关于用户的信息清单
self_history（我做过什么）—— 按需检索，镜像 events
  ┌ 给过的建议（→ 建议去重）
  ├ 做过的尝试（"提议每天练琴20分钟，他没坚持" → 教训）
  └ 失败/教训（"上次误以为他用了Rust" → 不确定先确认）
```

| 需求 | 靠哪块 |
|------|--------|
| 建议去重（不重复建议） | self_history 的"给过的建议" |
| 人格一致性（口癖/立场/行为承诺） | agent_model 行为准则（correct 更新） |
| 能力诚实（会什么/不会什么） | agent_model 能力 + 失败教训 |
| 元认知（知道缺口→主动问） | agent_model 认知缺口 + inventory 对照 |
| 关系状态（了解程度/禁区） | agent_model 关系 |

维护同档案：每轮 diff 增量更新（批进提取调用）；self_history 权重衰减同 episodic——**助手自己也会淡忘自己的失败，用户提起又重新激活**。
