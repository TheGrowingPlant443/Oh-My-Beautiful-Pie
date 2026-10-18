import unittest

import common
import lang_c
from testkit_syntax import (assert_node_kinds, by_id, caller_name, declarations, requires_grammar, run, sites, syms)

C_SRC = '''#include <stdio.h>
#include "local.h"
#define M(x) foo(x)
#define LIMIT 4
#if X
int cond(void) { return 1; }
#else
int cond(void) { return 2; }
#endif
static int (*getfp(int a))(int) { return 0; }
int *ptr(void) { return 0; }
int (*fp)(int) = 0;
void ex(void);
typedef struct { int a; } Pt;
typedef int (*cb_t)(int);
struct S { int (*cb)(int); };
int main(int argc, char **argv) { ex(); fp(1); s.cb(1); (*fp)(2); ptr(); main(0, 0); M(1); return 0; }
int shadow(int ptr) { return ptr(1); }
int ping(void) { return pong(); }
int pong(void) { return ping(); }
int unused(void) { return 0; }
int Größe(void) { return 1; }
int größe(void) { return 2; }
'''

CPP_SRC = '''#include <vector>
#include "x.hpp"
using namespace std;
using std::cout;
using Vec = std::vector<int>;
namespace al = ns;
namespace ns { namespace in {
class C : public B {
public:
  C() : x(0) {}
  ~C() {}
  void m() const { helper(); this->n(); ns::free(); auto l = [](int a){ return a; }; l(1); }
  int op() { return 1; }
  C operator+(const C& o) { return o; }
  void inl();
};
void C::inl() { m(); }
template <typename T> T tf(T t) { return t; }
}}
namespace { void hidden() {} }
int main() { ns::in::C c; c.m(); new C(); tf<int>(1); std::vector<int> v; v.push_back(1); return 0; }
'''

OBJC_SRC = '''#import <Foundation/Foundation.h>
#import "Local.h"
@import UIKit;
@protocol P
- (void)req;
@end
@interface Foo : NSObject
- (void)bar:(int)x baz:(int)y;
+ (instancetype)make;
@end
@implementation Foo
- (void)bar:(int)x baz:(int)y { [self other]; [Foo make]; [[NSObject alloc] init]; cfunc(1); [self bar:1 baz:2]; }
+ (instancetype)make { return nil; }
- (void)noargs { void (^blk)(void) = ^{ cfunc(2); }; blk(); }
@end
int cfunc(int a) { return a; }
'''


@requires_grammar("c")
class CRecipeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.r = run(lang_c, "c", C_SRC, "src/m.c")

    def test_profile_node_kinds_present_in_grammar(self):
        assert_node_kinds(self, lang_c, "c")

    def test_declarator_descent_distinguishes_functions_from_function_pointers(self):
        r = self.r
        names = {s["name"] for s in syms(r, "function")}
        self.assertTrue({"cond", "getfp", "ptr", "main", "shadow", "ping", "pong", "unused", "Größe", "größe"} <= names)
        self.assertNotIn("fp", names)            # int (*fp)(int) is a variable
        self.assertNotIn("cb", names)            # struct field function pointer
        self.assertEqual([s["name"] for s in syms(r, "signature")], ["ex"])
        self.assertEqual(declarations(r, None, "fp")[0]["kind"], "variable")
        gf = syms(r, "function", "getfp")[0]
        self.assertTrue(gf["bindings"]["static"])
        self.assertFalse(gf["exported"])
        self.assertEqual(gf["bindings"]["parameters"], ["a"])  # parameters of getfp, not the returned fp type

    def test_includes_binding(self):
        inc = {s["binding"]["specifier"]: s["binding"] for s in sites(self.r, "includes")}
        self.assertTrue(inc["stdio.h"]["system"])
        self.assertFalse(inc["local.h"]["system"])
        self.assertEqual(inc["local.h"]["kind"], "include")
        self.assertEqual(inc["local.h"]["relativeTo"], "dir")

    def test_computed_include_is_dynamic(self):
        r = run(lang_c, "c", "#define HDR \"a.h\"\n#include HDR\n", "x.c")
        inc = sites(r, "includes")[0]
        self.assertEqual(inc["resolution"], "dynamic")
        self.assertIsNone(inc["binding"]["specifier"])

    def test_macros_recorded_never_expanded(self):
        r = self.r
        macros = {s["name"]: s for s in syms(r, "macro")}
        self.assertEqual(set(macros), {"M", "LIMIT"})
        self.assertTrue(macros["M"]["bindings"]["functionLike"])
        self.assertEqual(macros["M"]["bindings"]["parameters"], ["x"])
        self.assertEqual(sites(r, "calls", "foo"), [])  # `foo(x)` inside the macro body is not a call fact
        use = [s for s in sites(r, "calls", "M")]
        self.assertEqual(len(use), 1)  # the use of M is an unresolved call-shaped site, not promoted
        self.assertIsNone(use[0]["targetId"])

    def test_conditional_compilation_flags_both_branches(self):
        conds = syms(self.r, "function", "cond")
        self.assertEqual(len(conds), 2)
        self.assertNotEqual(conds[0]["id"], conds[1]["id"])
        self.assertTrue(all(s["bindings"]["conditional"] for s in conds))
        self.assertNotIn("conditional", syms(self.r, "function", "ptr")[0]["bindings"])

    def test_include_guard_is_not_conditional(self):
        r = run(lang_c, "c", "#ifndef A_H\n#define A_H\nvoid f(void) { g(); }\n#endif\n", "a.h")
        self.assertNotIn("conditional", syms(r, "function", "f")[0]["bindings"])
        self.assertFalse(sites(r, "calls", "g")[0]["conditional"])

    def test_function_pointer_calls_are_dynamic_never_named_by_field(self):
        r = self.r
        field = [s for s in sites(r, "calls", "cb")][0]
        self.assertEqual(field["resolution"], "dynamic")
        self.assertEqual(field["binding"]["receiver"], "s")
        deref = [s for s in sites(r, "calls") if s["binding"]["kind"] == "dynamic"]
        self.assertEqual(len(deref), 1)
        fp_call = [s for s in sites(r, "calls", "fp")][0]
        self.assertEqual(fp_call["resolution"], "unresolved")  # lexical variable decl lets resolver see shadowing
        for s in sites(r, "calls"):
            self.assertIsNone(s["targetId"])

    def test_shadow_recursion_and_isolated(self):
        r = self.r
        pdecl = declarations(r, "shadow", "ptr")[0]
        call = [s for s in sites(r, "calls", "ptr") if caller_name(r, s) == "shadow"][0]
        self.assertEqual(pdecl["kind"], "parameter")
        self.assertTrue(pdecl["scopeStart"] <= call["span"]["byteStart"] < pdecl["scopeEnd"])
        self.assertEqual(caller_name(r, sites(r, "calls", "main")[0]), "main")
        self.assertEqual(caller_name(r, sites(r, "calls", "pong")[0]), "ping")
        self.assertEqual(sites(r, "calls", "unused"), [])
        self.assertTrue(syms(r, "function", "unused"))

    def test_typedef_anonymous_struct_takes_typedef_name(self):
        r = self.r
        self.assertEqual({s["name"] for s in syms(r, "struct")}, {"Pt", "S"})
        self.assertEqual({d["name"] for d in declarations(r, None) if d["kind"] == "type"}, {"Pt", "cb_t"})

    def test_unicode_case_variants_distinct(self):
        r = self.r
        a, b = syms(r, "function", "Größe")[0], syms(r, "function", "größe")[0]
        self.assertNotEqual(a["id"], b["id"])
        self.assertEqual(C_SRC.encode()[a["span"]["byteStart"]:a["span"]["byteEnd"]], "int Größe(void) { return 1; }".encode())
        again = run(lang_c, "c", C_SRC, "src/m.c")
        self.assertEqual([s["id"] for s in r["symbols"]], [s["id"] for s in again["symbols"]])

    def test_struct_reference_is_not_a_declaration(self):
        r = run(lang_c, "c", "void f(struct S *p) { struct T *q = 0; }\n")
        self.assertEqual(syms(r, "struct"), [])

    def test_unrecognizable_declarator_is_partial_not_silent(self):
        r = run(lang_c, "c", "int (*fp)(int) { return 0; }\n")
        self.assertTrue(r["partial"])

    def test_syntax_errors_partial(self):
        r = run(lang_c, "c", "int ok(void) { return 0; }\nint bad( {\n")
        self.assertTrue(r["partial"])
        self.assertTrue(syms(r, "function", "ok"))


@requires_grammar("cpp")
class CppRecipeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.r = run(lang_c, "cpp", CPP_SRC, "src/m.cpp")

    def test_profile_node_kinds_present_in_grammar(self):
        assert_node_kinds(self, lang_c, "cpp")

    def test_classes_methods_constructors_operators_templates(self):
        r = self.r
        self.assertEqual({s["qualifiedName"] for s in syms(r, "constructor")}, {"ns::in::C::C"})
        methods = {s["qualifiedName"] for s in syms(r, "method")}
        self.assertTrue({"ns::in::C::~C", "ns::in::C::m", "ns::in::C::op", "ns::in::C::operator+", "ns::in::C::inl"} <= methods)
        out_of_class = [s for s in syms(r, "method", "inl") if s["bindings"].get("qualifier") == "C"][0]
        self.assertEqual(out_of_class["bindings"]["qualifierKind"], "class")
        self.assertTrue(syms(r, "method", "~C")[0]["bindings"]["destructor"])
        self.assertEqual(syms(r, "class", "C")[0]["bindings"]["extends"], ["B"])
        tf = syms(r, "function", "tf")[0]
        self.assertTrue(tf["bindings"]["templated"])
        self.assertEqual([s["name"] for s in syms(r, "signature")], ["inl"])  # in-class prototype survives
        anon = [s for s in syms(r, "namespace") if s["name"] == ""][0]
        self.assertIn("<anonymous@", anon["qualifiedName"])
        self.assertFalse(syms(r, "function", "hidden")[0]["exported"])
        self.assertEqual(len(syms(r, "lambda")), 1)
        self.assertEqual(syms(r, "lambda")[0]["name"], "l")

    def test_using_and_alias_sites(self):
        r = self.r
        usings = {s["binding"]["specifier"]: s["binding"] for s in sites(r, "imports") if s["binding"]["kind"] == "using"}
        self.assertTrue(usings["std"]["namespace"])
        self.assertEqual(usings["std::cout"]["local"], "cout")
        alias = [s for s in sites(r, "imports") if s["binding"]["kind"] == "alias"][0]
        self.assertEqual((alias["binding"]["alias"], alias["binding"]["specifier"]), ("al", "ns"))

    def test_call_kinds_and_no_name_promotion(self):
        r = self.r
        member = sites(r, "calls", "m")
        self.assertTrue(any(s["binding"]["receiver"] == "c" and s["binding"]["kind"] == "member" for s in member))
        this = [s for s in sites(r, "calls", "n")][0]
        self.assertEqual((this["binding"]["receiverKind"], this["binding"]["operator"]), ("this", "->"))
        path = sites(r, "calls", "free")[0]
        self.assertEqual((path["binding"]["receiver"], path["binding"]["kind"]), ("ns", "path"))
        new = [s for s in sites(r, "calls") if s["binding"]["kind"] == "new"][0]
        self.assertEqual(new["binding"]["name"], "C")
        generic = sites(r, "calls", "tf")[0]
        self.assertTrue(generic["binding"]["generic"])
        for s in sites(r, "calls"):
            self.assertIsNone(s["targetId"])
            self.assertEqual(s["resolution"], "unresolved")  # C++ member/virtual calls are not claimed dynamic or resolved

    def test_cuda_and_metal_extensions_are_cpp_profile_with_explicit_partial(self):
        profile = lang_c.PROFILES["cpp"]["extensions"]
        for ext in (".cu", ".cuh", ".metal", ".hpp", ".cc", ".cxx", ".hh", ".hxx", ".cpp"):
            self.assertIn(ext, profile)
        cu = "__global__ void kern(float* p) { p[0] = 1; }\nvoid host() { kern<<<1,1>>>(0); }\n"
        r = run(lang_c, "cpp", cu, "k.cu")
        self.assertTrue(r["partial"])
        self.assertTrue(syms(r, "function", "kern"))
        self.assertTrue(any("'.cu'" in d and "not supported" in d for d in r["diagnostics"]))
        plain = run(lang_c, "cpp", "void f() { g(); }\n", "plain.cu")
        self.assertFalse(plain["partial"])

    def test_class_scope_function_pointer_member_and_lambda_pairing(self):
        r = run(lang_c, "cpp", "struct W { void (*cb)(); void go() { cb(); } };\n")
        decl = declarations(r, "W", "cb")[0]
        site = sites(r, "calls", "cb")[0]
        self.assertEqual(decl["kind"], "variable")
        self.assertTrue(decl["scopeStart"] <= site["span"]["byteStart"] < decl["scopeEnd"])

    def test_deterministic_ids(self):
        again = run(lang_c, "cpp", CPP_SRC, "src/m.cpp")
        self.assertEqual([s["id"] for s in self.r["symbols"]], [s["id"] for s in again["symbols"]])
        self.assertEqual([s["id"] for s in self.r["sites"]], [s["id"] for s in again["sites"]])


@requires_grammar("objc")
class ObjcRecipeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.r = run(lang_c, "objc", OBJC_SRC, "App/Foo.m")

    def test_profile_node_kinds_present_in_grammar(self):
        assert_node_kinds(self, lang_c, "objc")

    def test_selectors_classes_blocks(self):
        r = self.r
        methods = {s["qualifiedName"]: s for s in syms(r, "method")}
        self.assertEqual(set(methods), {"-[Foo bar:baz:]", "+[Foo make]", "-[Foo noargs]"})
        self.assertEqual(methods["-[Foo bar:baz:]"]["bindings"]["parameters"], ["x", "y"])
        self.assertFalse(methods["+[Foo make]"]["bindings"]["instance"])
        sigs = {s["qualifiedName"] for s in syms(r, "signature")}
        self.assertEqual(sigs, {"-[P req]", "-[Foo bar:baz:]", "+[Foo make]"})
        self.assertEqual({s["name"] for s in syms(r, "class")}, {"Foo"})
        self.assertEqual(syms(r, "interface")[0]["name"], "P")
        self.assertEqual(syms(r, "lambda")[0]["name"], "blk")
        self.assertTrue(syms(r, "function", "cfunc"))

    def test_imports_and_module_imports(self):
        r = self.r
        inc = {s["binding"]["specifier"]: s["binding"] for s in sites(r, "includes")}
        self.assertEqual(inc["Foundation/Foundation.h"]["directive"], "#import")
        mod = sites(r, "imports")[0]
        self.assertEqual((mod["binding"]["specifier"], mod["binding"]["module"]), ("UIKit", True))

    def test_messages_are_dynamic_and_selector_composed(self):
        r = self.r
        msgs = [s for s in sites(r, "calls") if s["binding"]["kind"] == "message"]
        self.assertEqual({s["binding"]["selector"] for s in msgs}, {"other", "make", "init", "alloc", "bar:baz:"})
        self.assertTrue(all(s["resolution"] == "dynamic" and s["targetId"] is None for s in msgs))
        self.assertEqual(next(s for s in msgs if s["binding"]["selector"] == "other")["binding"]["receiverKind"], "self")
        c_call = sites(r, "calls", "cfunc")
        self.assertEqual(len(c_call), 2)
        self.assertTrue(all(s["resolution"] == "unresolved" for s in c_call))

    def test_matlab_like_dot_m_is_partial_not_relabelled(self):
        r = run(lang_c, "objc", "function y = f(x)\n  y = x + 1;\nend\nz = f(2);\n", "tools/solve.m")
        self.assertTrue(r["partial"])
        self.assertTrue(any("ambiguous" in d and "MATLAB" in d for d in r["diagnostics"]))

    def test_clean_c_in_dot_m_without_objc_constructs_is_still_partial(self):
        r = run(lang_c, "objc", "int f(void) { return 0; }\n", "x.m")
        self.assertTrue(r["partial"])
        self.assertTrue(any("no Objective-C constructs" in d for d in r["diagnostics"]))

    def test_objcpp_cpp_constructs_are_explicit_partial(self):
        r = run(lang_c, "objc", "@implementation A\n- (void)f { std::vector<int> v; v.push_back(1); }\n@end\nnamespace n { void g() {} }\n", "x.mm")
        self.assertTrue(r["partial"])
        self.assertTrue(any("'.mm'" in d for d in r["diagnostics"]))


class CUnavailableTests(unittest.TestCase):
    def test_unknown_grammar_raises_instead_of_empty_success(self):
        ctx = common.Context("x.c", "not-a-language", b"int a(void){return 0;}\n")
        with self.assertRaises(common.GrammarUnavailable):
            lang_c.extract("c", b"int a(void){return 0;}\n", ctx)

    def test_profiles_extension_plan(self):
        self.assertEqual(lang_c.PROFILES["c"]["extensions"], [".c", ".h"])
        self.assertEqual(lang_c.PROFILES["objc"]["extensions"], [".m", ".mm"])


if __name__ == "__main__":
    unittest.main()
