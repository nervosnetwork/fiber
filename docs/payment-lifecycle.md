# Fiber Payment Lifecycle

This document provides a comprehensive visualization of the `PaymentSession` and `PaymentAttempt` lifecycles in the Fiber network.

## State Machine Overview

The following diagram illustrates the decoupled, event-driven transitions between the high-level payment intent (Session) and the individual routing attempts.

```mermaid
    stateDiagram-v2
        direction TB

        %% Entrance
        [*] --> Send: SendPayment (RPC)
        [*] --> Resume: RetrySendPayment / Reboot

        %% --- 1. Init && Build Route ---
        state PaymentSession_Created {
            direction LR
            New_Session --> Build_Route_Task: Amount Check & MPP Precheack
            note right of New_Session: 💾 insert_payment_session (if !dry_run)
        }
        Send --> PaymentSession_Created: delete_attempts & Idempotency Check

        Build_Route_Task --> Attempt_Created: Get Route
        Build_Route_Task --> PaymentSession_Failed: No Route Found

        %% --- 2. Attempt Execution ---
        state Attempt_Execution {
            Attempt_Created --> Attempt_Sent
            
            state tlc_result_choice <<choice>>
            Attempt_Sent --> tlc_result_choice: Wait Local ACK
            
            tlc_result_choice --> Attempt_Inflight: Ok
            tlc_result_choice --> Retry_Decision: WaitingTlcAck / Local Error
        }

        Attempt_Inflight --> PaymentSession_Inflight: calc_status

        %% --- 3. Result Determination and Retry  ---
        state Remote_Result <<choice>>
        Attempt_Inflight --> Remote_Result: Handle RemoveTlc

        Remote_Result --> Attempt_Success: Fulfill (Preimage)
        Remote_Result --> Retry_Decision: Fail (Error)

        state Retry_Decision <<choice>>
        
        Retry_Decision --> Attempt_Retrying: [Can Retry Now]
        Retry_Decision --> Attempt_Sent: [WaitingTlcAck] (Soft Lock)
        Retry_Decision --> Attempt_Failed: [Exhausted / Final Error]

        %% --- 4. State Synchronization and Final State ---
        Attempt_Success --> PaymentSession_Success: calc_status
        Attempt_Failed --> PaymentSession_Failed: calc_status

        Attempt_Retrying --> Build_Route_Task: Add Receiver Amount if last_error
        Attempt_Retrying --> Attempt_Sent
        Resume --> Attempt_Retrying: Check & Recovery

        %% Annotate Persistence Actions
        note right of PaymentSession_Success: 💾 update_payment_session (if !dry_run)
        note right of PaymentSession_Failed: 💾 set_failed_status (if !dry_run)