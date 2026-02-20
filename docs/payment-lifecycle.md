# Fiber Payment Lifecycle

This document provides a comprehensive visualization of the `PaymentSession` and `PaymentAttempt` lifecycles in the Fiber network.

## State Machine Overview

The following diagram illustrates the decoupled, event-driven transitions between the high-level payment intent (Session) and the individual routing attempts.

```mermaid
    flowchart TB
    subgraph PaymentSession [Payment Session]
        direction TB
        PS_Created[Created] --> PS_Inflight[Inflight]
        PS_Inflight -- "Σ(Settled Amount) == Total" --> PS_Success[Success / Finished]
        PS_Inflight -- "Exhausted / Terminal Error" --> PS_Failed[Failed]
        
        style PS_Success fill:#e1f5fe,stroke:#01579b
        style PS_Failed fill:#ffebee,stroke:#b71c1c
    end

    subgraph Attempts [Payment Attempts]
        direction TB
        A_Created[Attempt 1..N] --> A_Inflight[Inflight]
        A_Inflight --> A_Result{Result?}
        A_Result -- "Fulfill" --> A_Settled[Settled]
        A_Result -- "Fail (Retryable)" --> A_Retrying[Retrying]
        A_Result -- "Fail (Final)" --> A_Failed[Failed]
        
        A_Retrying --> A_Inflight
    end

    %% Aggregation & MPP Logic
    PS_Created -- "Splits Amount" --> A_Created
    A_Settled -- "Partial Amount Collected" --> PS_Inflight
    A_Settled -- "Full Amount Reached" --> PS_Success
    A_Failed -- "Cannot fulfill Total Amount" --> PS_Failed