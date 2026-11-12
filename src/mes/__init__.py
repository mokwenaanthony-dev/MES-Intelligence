"""MES-Intelligence: a small manufacturing execution system on an ISA-95 Unified Namespace.

It is ISA-95 Level 3 next to ERP-Intelligence (Level 4). The two share nothing but the Unified
Namespace contract (ERP repository, ``docs/UNS_CONTRACT.md``): the MES learns the plant's cells
and the work released to them from the ERP's retained messages, and reports the work done as
``operation-performance`` events. It keeps working when the ERP is down and resends unanswered
events until the ERP confirms them (store-and-forward).
"""
__version__ = "0.1.0"
CONTRACT_VERSION = 1  # the UNS contract version this MES implements
