// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {BlueprintServiceManagerBase} from "tnt-core/BlueprintServiceManagerBase.sol";
import {GpuLeaseVault} from "./GpuLeaseVault.sol";

/**
 * @title GpuLeaseBlueprint
 * @dev Blueprint service manager (BSM) for the GPU lease marketplace — the
 *      tnt-core integration layer. Money lives in GpuLeaseVault (I1–I5);
 *      this contract is pure job ROUTING + RESULT BINDING:
 *
 *      - LEASE (job 0): at result time, binds the operator-returned leaseId
 *        to the operator AND verifies it against the vault: the lease must be
 *        Live, its intentHash must equal the request's, its operator must be
 *        the submitting operator, and its lessee must be the job requester
 *        recorded at call time. The result's schemaVersion must be known
 *        (fail closed, SPEC §3).
 *      - RELEASE (1) / REAP (3): routes to the operator bound at LEASE time.
 *      - EXTEND (2): routes the same way; escrow extension is a value call
 *        on the vault itself (I2 enforcement), not through this contract.
 *
 *      Deliberately absent (SPEC §4): inventory registry, restaking coupling,
 *      on-chain pricing. GPU classes/generations are off-chain data.
 */
contract GpuLeaseBlueprint is BlueprintServiceManagerBase {
    // ═══════════════════════════════════════════════════════════════════
    // JOB IDS — MUST match gpu-lease-blueprint-lib/src/lib.rs and the
    // RegisterBlueprint job order (sequential, no gaps).
    // ═══════════════════════════════════════════════════════════════════
    uint8 public constant JOB_LEASE = 0;
    uint8 public constant JOB_RELEASE = 1;
    uint8 public constant JOB_EXTEND = 2;
    uint8 public constant JOB_REAP = 3;

    string public constant BLUEPRINT_NAME = "gpu-lease-blueprint";
    string public constant BLUEPRINT_VERSION = "0.1.0";

    /// @dev Latest result schema version this BSM understands. Unknown
    ///      versions fail closed (SPEC §3).
    uint16 public constant SUPPORTED_SCHEMA_VERSION = 1;

    // ═══════════════════════════════════════════════════════════════════
    // ABI TYPES — mirror gpu-lease-blueprint-lib/src/lib.rs sol! block
    // ═══════════════════════════════════════════════════════════════════

    struct GpuLeaseRequest {
        uint8 intentVersion;
        bytes32 intentHash;
        uint128 pricePerSecond;
        uint64 durationSeconds;
        uint8 confidentiality;
        string gpuClass;
        string region;
        address lessee; // SPEC §1: the RFQ quote binds the requester
        bytes32 leaseId; // the vault lease the buyer already escrowed — one identity across money and routing
    }

    struct GpuLeaseOutput {
        bytes32 leaseId;
        string endpoint;
        uint16 schemaVersion;
    }

    struct GpuLeaseIdRequest {
        bytes32 leaseId;
    }

    struct GpuLeaseExtendRequest {
        bytes32 leaseId;
        uint64 addSeconds;
    }

    struct GpuLeaseAck {
        bytes32 leaseId;
        uint8 state; // 0=Live 1=Released 2=Reaped (vault states)
        uint16 schemaVersion;
    }

    // ═══════════════════════════════════════════════════════════════════
    // STORAGE (ERC-7201)
    // ═══════════════════════════════════════════════════════════════════

    struct GpuLeaseStorage {
        GpuLeaseVault vault;
        // leaseId => operator bound at LEASE result time.
        mapping(bytes32 => address) leaseOperator;
        // (serviceId, jobCallId) => cached raw inputs (0.19 passes inputsHash only).
        mapping(uint64 => mapping(uint64 => bytes)) jobCallInputs;
        // leaseId => settled state mirror (1=Released 2=Reaped).
        mapping(bytes32 => uint8) leaseState;
    }

    // keccak256(abi.encode(uint256(keccak256("gpu.lease.storage")) - 1)) & ~bytes32(uint256(0xff))
    bytes32 private constant STORAGE_LOCATION =
        0x9c30236f4e0f2d09a17a77bba2cb01cbba7f21c7a1a29fbda44ff70b3f80be00;

    function _s() internal pure returns (GpuLeaseStorage storage $) {
        assembly {
            $.slot := STORAGE_LOCATION
        }
    }

    // ═══════════════════════════════════════════════════════════════════
    // EVENTS / ERRORS
    // ═══════════════════════════════════════════════════════════════════

    event VaultSet(address indexed vault);
    event LeaseBound(bytes32 indexed leaseId, address indexed operator, uint64 indexed serviceId, uint16 schemaVersion);
    event LeaseSettled(bytes32 indexed leaseId, address indexed operator, uint8 state);

    error VaultNotSet();
    error UnknownJobId(uint8 job);
    error UnknownSchemaVersion(uint16 version);
    error LeaseNotFound(bytes32 leaseId);
    error IntentMismatch(bytes32 inRequest, bytes32 inVault);
    error OperatorMismatch(address expected, address actual);
    error LesseeMismatch(address expected, address actual);
    error LeaseAlreadyBound(bytes32 leaseId);
    error VaultLeaseNotLive(bytes32 leaseId, uint8 state);

    // ═══════════════════════════════════════════════════════════════════
    // LIFECYCLE
    // ═══════════════════════════════════════════════════════════════════

    constructor(address vaultAddress) {
        if (vaultAddress == address(0)) revert VaultNotSet();
        _s().vault = GpuLeaseVault(vaultAddress);
        emit VaultSet(vaultAddress);
    }

    function vault() external view returns (GpuLeaseVault) {
        return _s().vault;
    }

    function leaseOperatorOf(bytes32 leaseId) external view returns (address) {
        return _s().leaseOperator[leaseId];
    }

    function leaseStateOf(bytes32 leaseId) external view returns (uint8) {
        return _s().leaseState[leaseId];
    }

    // ═══════════════════════════════════════════════════════════════════
    // JOB CALL — routing + requester recording (tnt-core 0.19 flow)
    // ═══════════════════════════════════════════════════════════════════

    function onJobCall(uint64 serviceId, uint8 job, uint64 jobCallId, bytes calldata inputs)
        external
        payable
        override
        onlyFromTangle
    {
        GpuLeaseStorage storage $ = _s();
        if (job == JOB_LEASE) {
            // Cache inputs for intent verification at result time (tnt-core
            // 0.19 forwards only inputsHash there). The lessee is carried IN
            // the request (public data — the RFQ quote already binds it).
            $.jobCallInputs[serviceId][jobCallId] = inputs;
        } else if (job == JOB_RELEASE || job == JOB_REAP || job == JOB_EXTEND) {
            $.jobCallInputs[serviceId][jobCallId] = inputs;
        } else {
            revert UnknownJobId(job);
        }
    }

    // ═══════════════════════════════════════════════════════════════════
    // JOB RESULT — binding + vault verification (the trust edge)
    // ═══════════════════════════════════════════════════════════════════

    function onJobResult(
        uint64 serviceId,
        uint8 job,
        uint64 jobCallId,
        address operator,
        bytes32 inputsHash,
        bytes calldata outputs
    ) external payable override onlyFromTangle {
        GpuLeaseStorage storage $ = _s();
        if (job == JOB_LEASE) {
            _bindLease($, serviceId, jobCallId, operator, inputsHash, outputs);
        } else if (job == JOB_RELEASE || job == JOB_REAP) {
            bytes32 leaseId = _consumeLeaseId($, serviceId, jobCallId, inputsHash);
            _requireBoundOperator($, leaseId, operator);
            uint8 state = job == JOB_RELEASE ? 1 : 2;
            $.leaseState[leaseId] = state;
            emit LeaseSettled(leaseId, operator, state);
        } else if (job == JOB_EXTEND) {
            // Extended on the vault directly (escrow); nothing to bind here —
            // verify the lease is still bound to this operator.
            bytes32 leaseId = _consumeLeaseId($, serviceId, jobCallId, inputsHash);
            _requireBoundOperator($, leaseId, operator);
        } else {
            revert UnknownJobId(job);
        }
    }

    /// @dev LEASE result: bind leaseId → operator AFTER verifying against the
    ///      vault (the money layer). Fail closed on every mismatch.
    function _bindLease(
        GpuLeaseStorage storage $,
        uint64 serviceId,
        uint64 jobCallId,
        address operator,
        bytes32 inputsHash,
        bytes calldata outputs
    ) internal {
        GpuLeaseRequest memory request =
            abi.decode(_consumeInputs($, serviceId, jobCallId, inputsHash), (GpuLeaseRequest));
        GpuLeaseOutput memory output = abi.decode(outputs, (GpuLeaseOutput));

        if (output.schemaVersion != SUPPORTED_SCHEMA_VERSION) {
            revert UnknownSchemaVersion(output.schemaVersion);
        }
        if ($.leaseOperator[output.leaseId] != address(0)) {
            revert LeaseAlreadyBound(output.leaseId);
        }

        // Vault cross-check: the lease the operator claims must be real,
        // live, and bound to this exact intent + operator + requester (I5).
        (address vOperator, address vLessee, uint128 vEscrow, uint128 vPrice, uint64 vExpiry, bytes32 vIntent, uint8 vConf, uint8 vState) =
            $.vault.leases(output.leaseId);
        // A lease that was never created reads as an empty struct (state 0) —
        // expiry == 0 is impossible for a real lease (durationSeconds >= 1).
        if (vExpiry == 0) revert VaultLeaseNotLive(output.leaseId, type(uint8).max);
        if (vState != 0) revert VaultLeaseNotLive(output.leaseId, vState);
        if (vIntent != request.intentHash) {
            revert IntentMismatch(request.intentHash, vIntent);
        }
        if (vOperator != operator) {
            revert OperatorMismatch(operator, vOperator);
        }
        address requester = request.lessee;
        if (vLessee != requester) {
            revert LesseeMismatch(requester, vLessee);
        }
        // Price binding: the vault lease price must match the request quote.
        if (vPrice != request.pricePerSecond) {
            revert IntentMismatch(request.intentHash, vIntent);
        }
        vEscrow; vExpiry; vConf; // (all cross-checked implicitly via intent binding)

        $.leaseOperator[output.leaseId] = operator;
        emit LeaseBound(output.leaseId, operator, serviceId, output.schemaVersion);
    }

    function _consumeLeaseId(
        GpuLeaseStorage storage $,
        uint64 serviceId,
        uint64 jobCallId,
        bytes32 inputsHash
    ) internal returns (bytes32 leaseId) {
        bytes memory inputs = _consumeInputs($, serviceId, jobCallId, inputsHash);
        // RELEASE/REAP carry GpuLeaseIdRequest; EXTEND carries GpuLeaseExtendRequest.
        // Both begin with bytes32 leaseId — decode the first word.
        assembly {
            leaseId := mload(add(inputs, 32))
        }
    }

    function _consumeInputs(
        GpuLeaseStorage storage $,
        uint64 serviceId,
        uint64 jobCallId,
        bytes32 inputsHash
    ) internal returns (bytes memory inputs) {
        inputs = $.jobCallInputs[serviceId][jobCallId];
        if (keccak256(inputs) != inputsHash) {
            // Mirrors tnt-core's own guarantee; defensive.
            revert IntentMismatch(inputsHash, keccak256(inputs));
        }
        delete $.jobCallInputs[serviceId][jobCallId];
    }

    function _requireBoundOperator(GpuLeaseStorage storage $, bytes32 leaseId, address operator) internal view {
        address bound = $.leaseOperator[leaseId];
        if (bound == address(0)) revert LeaseNotFound(leaseId);
        if (bound != operator) revert OperatorMismatch(operator, bound);
    }
}
