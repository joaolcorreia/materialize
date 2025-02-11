# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.


from materialize.checks.actions import Action, Initialize, Manipulate, Validate
from materialize.checks.mzcompose_actions import ConfigureMz, KillMz, StartMz
from materialize.checks.scenarios import Scenario
from materialize.util import MzVersion


class Node:
    def actions(self, scenario: Scenario) -> list[Action]:
        return []


class BeginUpgradeScenario(Node):
    def __repr__(self) -> str:
        return "BeginUpgradeScenario"


class EndUpgradeScenario(Node):
    def __repr__(self) -> str:
        return "EndUpgradeScenario"


class BeginVersion(Node):
    def __init__(self, version: MzVersion | None):
        self.version = version

    def __repr__(self) -> str:
        return f"BeginVersion({self.version})"

    def actions(self, scenario: Scenario) -> list[Action]:
        # As this action may need start very old Mz versions,
        # we do not use any bootstrap_systme_parameters
        return [
            StartMz(tag=self.version, system_parameter_defaults={}),
            ConfigureMz(scenario),
        ]


class EndVersion(Node):
    def __init__(self, version: MzVersion | None):
        self.version = version

    def __repr__(self) -> str:
        return f"EndVersion({self.version})"

    def actions(self, scenario: Scenario) -> list[Action]:
        return [KillMz()]


class ChecksInitialize(Node):
    def __repr__(self) -> str:
        return "ChecksInitialize"

    def actions(self, scenario: Scenario) -> list[Action]:
        return [Initialize(scenario)]


class ChecksManipulate1(Node):
    def __repr__(self) -> str:
        return "ChecksManipulate(#1)"

    def actions(self, scenario: Scenario) -> list[Action]:
        return [Manipulate(scenario, phase=1)]


class ChecksManipulate2(Node):
    def __repr__(self) -> str:
        return "ChecksManipulate(#2)"

    def actions(self, scenario: Scenario) -> list[Action]:
        return [Manipulate(scenario, phase=2)]


class ChecksValidate(Node):
    def __repr__(self) -> str:
        return "ChecksValidate"

    def actions(self, scenario: Scenario) -> list[Action]:
        return [Validate(scenario)]
