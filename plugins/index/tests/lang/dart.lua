local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local has = helpers.has
local lacks = helpers.lacks

case("dart_all_sections", function()
  local src = [==[
library my_app;

import 'dart:core';
import 'package:flutter/widgets.dart';
export 'src/utils.dart';
part 'src/part.dart';

const MAX_SIZE = 100;

typedef StringList = List<String>;

class User extends Entity implements Comparable<User> {
  String name;
  int age;

  User(this.name, this.age);

  String greet() => 'Hello $name';

  static User create() => User('', 0);
}

mixin Loggable on Base {
  void log(String msg);
}

extension StringExt on String {
  String quoted() => '"$this"';
}

enum Color {
  red,
  green,
  blue,
}

void main() {
  print('hello');
}

int add(int a, int b) => a + b;

get value => _value;
set value(int v) => _value = v;
]==]
  local out = idx(src, "dart")
  has(out, {
    "mod:",
    "my_app",
    "imports:",
    "dart.core",
    "flutter.widgets",
    "export:",
    "part:",
    "consts:",
    "MAX_SIZE",
    "types:",
    "StringList",
    "classes:",
    "User",
    "greet",
    "create",
    "traits:",
    "Loggable",
    "Base",
    "impls:",
    "extension StringExt",
    "String",
    "Color",
    "fns:",
    "main",
    "add",
    "get value",
    "set value",
  })
  lacks(out, {
    "interfaces:",
  })
end)

case("dart_extension_type", function()
  local src = [==[
extension type Id(int value) {
  Id.fromStr(String s) : value = int.parse(s);
  int get v => value;
}
]==]
  local out = idx(src, "dart")
  has(out, {
    "classes:",
    "extension type Id",
  })
end)

case("dart_enum_with_interfaces", function()
  local src = [==[
enum Status implements Comparable<Status> {
  active,
  inactive,
  pending,
}
]==]
  local out = idx(src, "dart")
  has(out, {
    "types:",
    "enum Status",
    "active",
    "inactive",
    "pending",
  })
end)

case("dart_import_with_alias", function()
  local src = [==[
import 'dart:math' as math;
import 'package:flutter/widgets.dart' show Widget, BuildContext hide Deprecated;
]==]
  local out = idx(src, "dart")
  has(out, {
    "imports:",
    "dart.math",
    "as math",
    "flutter.widgets",
  })
end)
