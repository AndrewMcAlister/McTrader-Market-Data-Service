Core Principles
1. Complete Replacements, Not Coexistence

When superseding functionality, remove the old implementation entirely.

Do not allow old and new implementations to coexist, as this creates redundancy, confusion, maintenance overhead, and divergent behavior.

For code that we own and control:

No fallbacks
No legacy paths
No feature duplication

For external dependencies that may be unavailable or unreliable (market data, broker APIs, databases, web services, third-party systems), fallbacks and resiliency mechanisms are encouraged.

2. Write Atomic Functions and Methods

Functions and methods should perform one clearly defined responsibility.

A method that performs multiple distinct operations should be split into smaller units.

Good examples:

Python
build_order_request()
validate_order()
submit_order()
persist_trade()
Show more lines

Poor examples:

Python
validate_and_submit_order()
load_process_and_save_data()
Show more lines
3. Prefer Strong and Explicit Types

Avoid:

Python
Any
dict
list
Show more lines

when the structure is known.

Prefer:

Dataclasses
TypedDict
NamedTuple
Enums
Concrete collections
Explicit type aliases

Example:

Python
@dataclass
class OrderResult:
order_id: str
status: OrderStatus
filled_quantity: float
Show more lines

The more specific the type definition, the better the IDE support, static analysis, and maintainability.

4. Use Composition Before Complexity

Look for opportunities to use:

Composition
Inheritance
Strategy patterns
Collaborator classes

Avoid creating monolithic classes responsible for multiple concerns.

Favor focused components that can be composed together.

5. Keep Methods and Functions Short

Methods should be short enough to understand without scrolling.

As a guideline:

Functions over 50 lines should be reviewed.
Large methods are usually hiding multiple responsibilities.

If a function requires extensive comments to explain its flow, it likely needs decomposition.

6. Apply Encapsulation and Single Responsibility

Every class should have a single reason to change.

Classes should expose behavior through public interfaces and hide implementation details.

Avoid exposing internal state or testing implementation details directly.

Good:

Python
portfolio.calculate_exposure()
Show more lines

Bad:

Python
portfolio._calculate_exposure()
Show more lines
7. Prefer Stateless Functions

If logic does not require:

self
cls
object state

then it should be implemented as a standalone function.

Pure functions are:

Easier to test
Easier to reuse
Easier to reason about
Less vulnerable to side effects
8. Separate Business Logic from I/O

Keep business rules independent from external systems.

Separate:

Validation
Transformation
Calculations
Rule evaluation

from:

Database access
Broker calls
Network requests
File access
API integrations

Business logic should remain testable without requiring external dependencies.

9. Use Descriptive Single-Purpose Names

Method names should clearly communicate a single action.

Prefer verbs such as:

Python
build_
validate_
calculate_
transform_
fetch_
persist_
load_
save_
create_
remove_
Show more lines

Method names containing "and" are often indicators that multiple responsibilities exist and should be reviewed.

10. Design Small Focused Classes

Avoid God Objects and God Classes.

Large classes should be decomposed into focused collaborators.

Example:

Instead of:

Python
TradeManager
Show more lines

handling:

Validation
Risk checks
Broker execution
Persistence
Notifications

Split responsibilities into:

Python
TradeValidator
RiskManager
BrokerExecutor
TradeRepository
NotificationService
Show more lines
11. Eliminate Duplicate Logic

Any logic appearing in multiple locations should be consolidated.

Extract common behavior into:

Utilities
Services
Base classes
Shared modules

There should be one authoritative implementation of a rule or calculation.

12. Program Against Abstractions, Return Concrete Results

Internal design should favor abstractions where appropriate.

Outputs should be concrete and well-defined.

Prefer:

Python
OrderResult
PortfolioExposure
TradeSummary
Show more lines

over:

Python
dict[str, Any]
Show more lines
13. Test Behavior, Not Implementation Details

Tests should verify externally observable behavior.

Avoid testing:

Python
_private_method()
_internal_state
Show more lines

unless there is an exceptional reason.

Tests should remain stable when implementations are refactored.

14. Validate Before Expensive Operations

Perform prerequisite checks before initiating expensive or potentially blocking work.

Examples:

Verify database type before connecting.
Verify broker credentials before API calls.
Verify configuration before starting background tasks.
Verify file existence before processing.

Fail fast whenever possible.

15. Every Reusable Component Requires Tests

All reusable utilities, services, helpers, and business logic should have dedicated unit tests.

New functionality is not complete until it has automated test coverage.

16. Remove Dead Code Immediately

Do not leave behind:

Unused methods
Deprecated implementations
Commented-out code
Unused imports
Stale interfaces
Abandoned helper functions

Dead code increases maintenance costs and creates confusion.

17. Minimize Side Effects

Avoid hidden state mutations and unexpected behavior.

A caller should easily understand:

What a function consumes
What it returns
What external effects it creates

Side effects should be explicit.

18. Optimize for Readability

Code is read far more often than it is written.

Prefer:

Clear names
Clear structure
Simple control flow
Explicit behavior

over clever, condensed, or highly abstract implementations.

If a solution requires significant explanation, consider simplifying it.

19. Maintain a Single Source of Truth

Business rules, calculations, constants, mappings, and configuration should exist in exactly one location.

Avoid duplicating:

Python
FEE_PERCENTAGE = 0.0025
Show more lines

across multiple files.

Centralize shared logic and shared configuration.

20. Verify Before Completion

A task is not complete until:

Syntax has been validated.
Existing tests pass.
New tests pass.
Dead code has been removed.
Call sites have been updated.
Type checking passes where applicable.
Temporary files and used one-off script files have beed deleted

Changes should leave the codebase cleaner than it was before the modification.

Guiding Philosophy

Write code that is easy to understand, easy to test, easy to modify, and difficult to misuse.

Prioritize:

Correctness
Simplicity
Readability
Maintainability
Performance

Only optimize for performance after correctness and clarity have been achieved.

21. When a plan is finished, delete the plan, but any important CODING (not config) parts that were deferred should be added to TODO/Loose-Ends.md separated by a line of ********************************

22. When creating database tables, don't automatically use the largest fields available, use the minimum size with about 50% headroom.  Eg only use varchar(max) for really large json columns.  Most json can be held comfortably in a varchar(5000).
Only use nvarchar if international characters might be stored.

23. Maintain the guard catalogue.

A guard is gating logic that is more than a one-line calculation: a rule that can block, veto, cap, clamp, delay or force a trading action, and that reads better as a named function or method than as several lines of code inline.

- Every guard has exactly one entry in the catalogue (ai_traders/agent_modules/guard_catalogue.py): its name, where it lives, what it gates, what fires it, what clears it, its configuration key where it has one, its tests, and a note of about five lines describing it.
- A guard is not complete until it is catalogued. Several lines of gating logic sitting inline in a longer method should be extracted into a named function or method and catalogued; the catalogue doubles as the worklist for that extraction.
- A true one-liner stays inline and is not catalogued. A catalogue padded with trivia stops being trustworthy.
- The catalogue is an index, not a home. Guard code stays with the collaborator that owns the state it reads (principles 4 and 10); the catalogue must never become the god class.
- A test keeps the catalogue honest: an entry whose location no longer exists, or a known guard with no entry, fails the suite.

24. All configuration keys in environment and broker config yaml files and the live broker config rows in the database should be all upper case and if they represent a percentage then the key should end in _PERCENT and the value should be double-quoted ending with a '%' eg "5.0%" or "17%"

25. Reasoning Requirements
Always use high reasoning effort.

Before modifying code:

Read all affected files.
Trace call chains.
Check for duplicate implementations.
Verify dependencies.
Search for existing abstractions.
Do not implement until architecture is understood.