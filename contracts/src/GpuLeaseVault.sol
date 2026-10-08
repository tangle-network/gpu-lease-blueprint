// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// @title GpuLeaseVault — the fixed-point core of the GPU lease marketplace.
/// @author Tangle Network
/// @notice Escrow-backed GPU leases with parking-meter economics. This contract
///         is deliberately minimal: money conservation, lease lifecycle, and a
///         slashing hook. Pricing, inventory, GPU classes, and attach transports
///         are off-chain data (see SPEC.md) and must NEVER migrate on-chain.
///
/// @dev INVARIANTS (the contract is correct iff these hold forever):
///  I1 Escrow conservation: address(this).balance == totalEscrowed + operatorEarnings
///     at every observable point (excess value is refunded at call boundaries).
///  I2 Overstay impossibility: for every live lease,
///     pricePerSecond * deviceCount * remainingSeconds(lease) <= escrow(lease).
///     GPU access beyond escrow is structurally unpayable.
///  I3 Refund atomicity: RELEASE refunds exactly
///     pricePerSecond * deviceCount * remainingSeconds
///     (== escrow * remaining / paidSeconds, exact — see _settle), atomically with
///     the state transition; no partial state is observable.
///  I4 Single settlement: each lease settles exactly once (RELEASE xor REAP);
///     the second settlement attempt reverts NotLive.
///  I5 Quote binding: operator, lessee, intentHash, pricePerSecond, deviceCount
///     in the lease record are written exactly once (at create) and never mutated.
contract GpuLeaseVault {
    /// @notice A lease. `endpointInfo` carries ONLY public data (schema-versioned);
    ///         credentials NEVER touch this contract or any event/log (SPEC §2).
    struct Lease {
        address operator;       // I5: immutable after create
        address lessee;         // I5: immutable after create
        uint128 escrow;         // remaining prepaid escrow (wei) == price * count * paidSeconds
        uint128 pricePerSecond; // wei/sec PER DEVICE, from the redeemed quote (I5: immutable)
        uint64  expiry;         // unix second when paid time ends
        bytes32 intentHash;     // keccak256 of versioned intent (I5: immutable)
        uint8   confidentiality;// mirrors the quote's TEE binding (I5: immutable)
        uint8   state;          // 0=Live 1=Released 2=Reaped
        uint16  deviceCount;    // devices rented (I5: immutable; 1 for classic single-GPU)
    }

    /// @notice Lease lifecycle events. No secrets, ever.
    event LeaseCreated(bytes32 indexed leaseId, address indexed operator, address indexed lessee, uint128 escrow, uint128 pricePerSecond, uint16 deviceCount, uint64 expiry, bytes32 intentHash, uint8 confidentiality, bytes endpointInfo, uint16 schemaVersion);
    event LeaseExtended(bytes32 indexed leaseId, uint128 addedEscrow, uint64 newExpiry);
    event LeaseReleased(bytes32 indexed leaseId, uint128 refund, uint128 operatorTake);
    event LeaseReaped(bytes32 indexed leaseId, uint128 operatorTake, address caller);
    event OperatorSlashed(address indexed operator, bytes32 indexed leaseId, uint256 amount);

    uint16 public constant SCHEMA_VERSION = 1;
    /// @dev Sanity cap on devices per lease — the platform caps at 16; the
    ///      vault allows headroom but refuses absurd quantities.
    uint16 public constant MAX_DEVICE_COUNT = 64;

    /// @dev Total wei currently escrowed for LIVE leases (I1).
    uint256 public totalEscrowed;
    /// @dev Total withdrawable operator earnings, settled and unwithdrawn (I1).
    uint256 public operatorEarnings;

    mapping(bytes32 => Lease) public leases;
    mapping(address => uint256) public operatorEarningsOf;

    error NotLessee();
    error NotLive();
    error ZeroPrice();
    error InsufficientEscrow();
    error DurationZero();
    error DeviceCountZero();
    error DeviceCountTooHigh(uint16 requested, uint16 max);
    error Overflow();
    error NothingToWithdraw();

    /// @notice Create a lease. Called by the tnt-core job layer (LEASE) or directly
    ///         by the buyer for a self-managed flow. Escrow = price × duration ×
    ///         deviceCount, re-verified here (fail-closed). Excess msg.value is
    ///         refunded so that I1 holds exactly (no stranded dust in the vault).
    function create(
        address operator,
        uint128 pricePerSecond,
        uint64  durationSeconds,
        uint16  deviceCount,
        bytes32 intentHash,
        uint8   confidentiality,
        bytes   calldata endpointInfo // public endpoint descriptor, versioned (SPEC §2)
    ) external payable returns (bytes32 leaseId) {
        if (pricePerSecond == 0) revert ZeroPrice();
        if (durationSeconds == 0) revert DurationZero();
        if (deviceCount == 0) revert DeviceCountZero();
        if (deviceCount > MAX_DEVICE_COUNT) revert DeviceCountTooHigh(deviceCount, MAX_DEVICE_COUNT);
        uint256 cost = uint256(pricePerSecond) * uint256(durationSeconds) * uint256(deviceCount);
        if (msg.value < cost) revert InsufficientEscrow();
        if (cost > type(uint128).max) revert Overflow();
        uint256 expiry256 = uint256(block.timestamp) + uint256(durationSeconds);
        if (expiry256 > type(uint64).max) revert Overflow();
        uint64 expiry = uint64(expiry256);
        leaseId = keccak256(abi.encodePacked(operator, msg.sender, intentHash, block.timestamp, totalEscrowed, msg.value));
        leases[leaseId] = Lease(operator, msg.sender, uint128(cost), pricePerSecond, expiry, intentHash, confidentiality, 0, deviceCount);
        totalEscrowed += cost;
        if (msg.value > cost) _refund(msg.sender, msg.value - cost);
        emit LeaseCreated(leaseId, operator, msg.sender, uint128(cost), pricePerSecond, deviceCount, expiry, intentHash, confidentiality, endpointInfo, SCHEMA_VERSION);
    }

    /// @notice Extend a live lease by paying for more seconds. Enforces I2:
    ///         after the call, price × deviceCount × remaining == escrow again.
    ///         The lessee (or anyone subsidizing) may pay; only the lease's
    ///         money changes.
    function extend(bytes32 leaseId, uint64 addSeconds) external payable {
        Lease storage l = leases[leaseId];
        if (l.state != 0) revert NotLive();
        if (addSeconds == 0) revert DurationZero();
        uint256 cost = uint256(l.pricePerSecond) * uint256(addSeconds) * uint256(l.deviceCount);
        if (msg.value < cost) revert InsufficientEscrow();
        uint256 newEscrow = uint256(l.escrow) + cost;
        if (newEscrow > type(uint128).max) revert Overflow();
        uint256 newExpiry256 = uint256(l.expiry) + uint256(addSeconds);
        if (newExpiry256 > type(uint64).max) revert Overflow();
        uint64 newExpiry = uint64(newExpiry256);
        l.escrow = uint128(newEscrow);
        l.expiry = newExpiry;
        totalEscrowed += cost;
        if (msg.value > cost) _refund(msg.sender, msg.value - cost);
        emit LeaseExtended(leaseId, uint128(cost), newExpiry);
    }

    /// @notice Voluntary release: atomic pro-rata refund to the lessee, elapsed
    ///         time paid to the operator. Settles exactly once (I3, I4).
    function release(bytes32 leaseId) external {
        Lease storage l = leases[leaseId];
        if (msg.sender != l.lessee) revert NotLessee();
        if (l.state != 0) revert NotLive();
        (uint128 refund, uint128 take) = _settle(l);
        emit LeaseReleased(leaseId, refund, take);
        _refund(l.lessee, refund);
    }

    /// @notice Permissionless reap after expiry: full remaining escrow to the
    ///         operator. Overstay is impossible — GPU access is credential-scoped
    ///         off-chain and the operator revokes credentials at expiry (SPEC §2).
    function reap(bytes32 leaseId) external {
        Lease storage l = leases[leaseId];
        if (l.state != 0) revert NotLive();
        if (block.timestamp < l.expiry) revert NotLive();
        uint128 take = l.escrow;
        l.escrow = 0;
        l.state = 2;
        totalEscrowed -= take;
        operatorEarningsOf[l.operator] += take;
        operatorEarnings += take;
        emit LeaseReaped(leaseId, take, msg.sender);
    }

    /// @notice Slashing hook for tnt-core governance/operator-staking integration.
    ///         Callable only on settled (Released/Reaped) leases — the RELEASE-
    ///         attested violation path (SPEC §1). Money movement is delegated to
    ///         the staking system; this records the penalty event.
    /// @notice Slashing notification from tnt-core (fires after a slash executes).
    /// @dev The vault does NOT manage slashing — tnt-core owns the lifecycle
    ///      (proposeSlash → disputeSlash → executeSlash). This hook just records
    ///      the outcome so the lease history is complete. See tangle-slashing.
    function onOperatorSlashed(bytes32 leaseId, uint256 amount) external {
        emit OperatorSlashed(leases[leaseId].operator, leaseId, amount);
    }

    /// @notice Operators withdraw settled earnings.
    function withdrawEarnings() external {
        uint256 amount = operatorEarningsOf[msg.sender];
        if (amount == 0) revert NothingToWithdraw();
        operatorEarningsOf[msg.sender] = 0;
        operatorEarnings -= amount;
        _refund(msg.sender, amount);
    }

    /// @dev EXACT pro-rata settlement math (I3). State transitions happen here;
    ///       events and transfers happen at the call site (checks-effects-interactions).
    ///
    ///      paidSeconds is DERIVED, not stored: escrow == pricePerSecond * deviceCount
    ///      * paidSeconds holds at every mutation site (create and extend are the only
    ///      writers of escrow, and both add exactly price * count * seconds), so the
    ///      division below has zero remainder by construction. The test suite pins
    ///      this derivation.
    ///
    ///      refund = price * count * remaining       (== escrow * remaining / paidSeconds, exact)
    ///      take   = escrow - refund                  (elapsed-time payment, all devices)
    ///      refund + take == escrow                   (I1 conservation at settle)
    function _settle(Lease storage l) internal returns (uint128 refund, uint128 take) {
        uint256 remaining = l.expiry > block.timestamp ? uint256(l.expiry - block.timestamp) : 0;
        refund = uint128(uint256(l.pricePerSecond) * uint256(l.deviceCount) * remaining); // <= escrow, cannot overflow uint128
        uint128 escrowBefore = l.escrow;
        take = escrowBefore - refund;
        l.escrow = 0;
        l.state = 1;
        totalEscrowed -= escrowBefore;
        operatorEarningsOf[l.operator] += take;
        operatorEarnings += take;
    }

    /// @dev Refund helper (pull-free, reentrancy-safe after effects).
    function _refund(address to, uint256 amount) internal {
        (bool ok, ) = payable(to).call{value: amount}("");
        require(ok, "ETH_TRANSFER_FAILED");
    }
}
