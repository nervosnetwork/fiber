# Trampoline Routing 设计说明（含 Fee 规则）

> 本文基于当前实现（graph.rs / payment.rs）描述 trampoline routing 的工作原理与 fee 计算。

## 1. 原理概述

Trampoline routing 的核心思路是：
- 发送方只需要找到**到首个 trampoline 节点**的可用路由；
- 之后的路由由 trampoline 节点继续构建并转发；
- 内层 trampoline onion 负责编码后续 trampoline 链以及最终收款方。

因此，发送方的外层 onion 只到首个 trampoline，后续转发能力由 trampoline 自行完成。

## 2. RPC 参数说明（与 trampoline 相关）

### SendPaymentCommand（关键字段）
- `invoice`：如果提供 invoice，优先使用其中的支付信息。
- `target_pubkey`：目标节点公钥（keysend 或无 invoice 场景）。
- `amount`：支付金额（keysend 或无 invoice 场景）。
- `keysend`：是否使用 keysend。
- `max_fee_amount`：**总 fee 预算**。用于限制 trampoline service fee + 路由 fee。
- `trampoline_hops`：显式 trampoline 路径（必填/非空时触发 trampoline routing）。
- `final_tlc_expiry_delta`：最终 hop 的 tlc expiry delta。
- `tlc_expiry_limit`：整条路径的 tlc expiry 上限。
- `max_parts`：MPP 拆分相关（若开启）。

### trampoline_hops 每一项
- `pubkey`：trampoline 节点公钥。
- `fee_rate`：**trampoline service fee** 费率（ppm，可选）。
  - 未提供时使用 `DEFAULT_FEE_RATE * 2`。
- `tlc_expiry_delta`：该 trampoline hop 的 expiry delta（可选，默认 `DEFAULT_TLC_EXPIRY_DELTA`）。

### 约束（校验）
- `trampoline_hops` 必须非空。
- 数量上限：`MAX_TRAMPOLINE_HOPS_LIMIT = 5`。
- 不能包含 `target_pubkey`。
- 不能重复。
- 首个 trampoline 不能是 source 或 target。
- 必须支持 trampoline routing。

## 3. Fee 计算规则

### 3.1 Trampoline Service Fee（服务费）
从尾到头逐跳计算：
1. 初始 `next_amount_to_forward = final_amount`。
2. 对每个 trampoline hop（从后往前）：
   - 计算该 hop 的 trampoline service fee：

   $$
   fee = round\\_up\left(\frac{amount\\_to\\_forward \times fee\\_rate}{1{,}000{,}000}\right)
   $$

   - 累加到 `next_amount_to_forward`。
3. 得到 `amount_to_first_trampoline`。
4. `trampoline_service_fee_total = amount_to_first_trampoline - final_amount`。

`fee_rate` 缺省时使用 `DEFAULT_FEE_RATE * 2`。

### 3.2 路由费预算（Routing Budget）
- **总预算**：`max_fee_amount`。
- **剩余预算**：

$$
remaining\\_budget = max\\_fee\\_amount - trampoline\\_service\\_fee\\_total
$$

该 `remaining_budget` 用于：
- 发送方到首个 trampoline 的路由 fee 预算
- 每个 trampoline 到下一跳的路由 fee 预算

### 3.3 预算分配规则（当前实现：均分）
- 预算分成 `hops.len() + 1` 份：
  - index 0：发送方到首个 trampoline
  - index i+1：第 i 个 trampoline 的路由预算
- 平均分配：

$$
base = \left\lfloor \frac{remaining\\_budget}{slots} \right\rfloor
$$

- 余数按顺序（从 index 0 开始）逐个 +1。

### 3.4 `build_max_fee_amount`
- 对每个 trampoline hop，将其分到的路由预算写入 `build_max_fee_amount`。
- 该值用于 trampoline 节点构建外层路由时的 max fee 约束。

### 3.5 默认/推荐 fee
当 `max_fee_amount` 未指定时：
- 计算单跳推荐路由 fee：

$$
single\\_forward\\_fee = round\\_up\left(\frac{amount\\_to\\_first\\_trampoline \times DEFAULT\\_FEE\\_RATE}{1{,}000{,}000}\right)
$$

- 最小推荐：

$$
default\\_min\\_fee = trampoline\\_service\\_fee\\_total + hops\\_len \times single\\_forward\\_fee
$$

- 最大推荐：

$$
default\\_max\\_fee = trampoline\\_service\\_fee\\_total + hops\\_len \times single\\_forward\\_fee \times 10
$$

实现细节：
- 若 `max_fee_amount` 未设置，则使用 `default_min_fee_amount` 作为总预算。
- 若设置了 `max_fee_amount`，但小于 `default_min_fee_amount`，会报错并提示推荐区间。

## 4. tlc expiry 规则（trampoline 专用）
- trampoline 的 `tlc_expiry_delta` 会逐跳累加。
- 计算得到的总 delta 不得超过 `tlc_expiry_limit`，否则报错。

## 5. 小结
- **Trampoline service fee**：由 hop fee_rate 决定，自尾向头累加。
- **Routing budget**：`max_fee_amount - service_fee_total`，当前实现均分。
- **默认/推荐**：未设置 `max_fee_amount` 时使用 `default_min_fee_amount`；若设置过低会报错并给出推荐区间。
- **限制**：trampoline hop 数量最多 5；普通 onion hop 数量受包大小约束（39/40 单测验证）。
