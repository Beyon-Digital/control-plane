#!/usr/bin/env python3
"""Generate the universal Apple application project with pinned package dependencies."""
from pathlib import Path
import hashlib

root = Path(__file__).resolve().parent
project = root / "ControlPlane.xcodeproj"
project.mkdir(exist_ok=True)

def ident(value):
    return hashlib.sha256(value.encode()).hexdigest()[:24].upper()

sources = sorted((root / "Sources").rglob("*.swift"))
objects = []
def obj(name, body):
    value=f"{ident(name)} = {{ {body} }};"
    existing=next((index for index,line in enumerate(objects) if line.startswith(ident(name)+" =")),None)
    if existing is None:objects.append(value)
    else:objects[existing]=value
    return ident(name)

file_ids, build_ids = [], []
for path in sources:
    relative = str(path.relative_to(root))
    file_id = obj(relative, f'isa = PBXFileReference; lastKnownFileType = sourcecode.swift; path = "{relative}"; sourceTree = "<group>";')
    build_id = obj("build:" + relative, f"isa = PBXBuildFile; fileRef = {file_id};")
    file_ids.append(file_id); build_ids.append(build_id)
resource_build_ids=[]
for relative,file_type in [("Assets.xcassets","folder.assetcatalog"),("Sources/ControlPlaneApp/PrivacyInfo.xcprivacy","text.xml")]:
 file_id=obj(relative,f'isa = PBXFileReference; lastKnownFileType = {file_type}; path = "{relative}"; sourceTree = "<group>";')
 file_ids.append(file_id)
 resource_build_ids.append(obj("resource:"+relative,f'isa = PBXBuildFile; fileRef = {file_id};'))
app = obj("app", 'isa = PBXFileReference; explicitFileType = wrapper.application; path = ControlPlane.app; sourceTree = BUILT_PRODUCTS_DIR;')
products = obj("products", f'isa = PBXGroup; name = Products; children = ({app},); sourceTree = "<group>";')
group = obj("group", f'isa = PBXGroup; children = ({",".join(file_ids + [products])},); sourceTree = "<group>";')
phase = obj("sources", f'isa = PBXSourcesBuildPhase; buildActionMask = 2147483647; files = ({",".join(build_ids)},); runOnlyForDeploymentPostprocessing = 0;')
package_ids, package_products, framework_ids = [], [], []
for package_name, url, version, products_for_package in [
 ("grpc-swift-2", "https://github.com/grpc/grpc-swift-2.git", "2.4.4", ["GRPCCore"]),
 ("grpc-swift-nio-transport", "https://github.com/grpc/grpc-swift-nio-transport.git", "2.11.0", ["GRPCNIOTransportHTTP2"]),
 ("grpc-swift-protobuf", "https://github.com/grpc/grpc-swift-protobuf.git", "2.5.0", ["GRPCProtobuf"]),
 ("swift-protobuf", "https://github.com/apple/swift-protobuf.git", "1.39.0", ["SwiftProtobuf"]),
 ("SwiftTerm", "https://github.com/migueldeicaza/SwiftTerm.git", "1.19.0", ["SwiftTerm"]),
]:
 package_id = obj("package:"+package_name, f'isa = XCRemoteSwiftPackageReference; repositoryURL = "{url}"; requirement = {{ kind = exactVersion; version = {version}; }};')
 package_ids.append(package_id)
 for product in products_for_package:
  product_id = obj("product:"+product, f'isa = XCSwiftPackageProductDependency; package = {package_id}; productName = {product};')
  package_products.append(product_id)
  framework_ids.append(obj("framework:"+product, f'isa = PBXBuildFile; productRef = {product_id};'))
frameworks = obj("frameworks", f'isa = PBXFrameworksBuildPhase; buildActionMask = 2147483647; files = ({",".join(framework_ids)},); runOnlyForDeploymentPostprocessing = 0;')
resources = obj("resources", f'isa = PBXResourcesBuildPhase; buildActionMask = 2147483647; files = ({",".join(resource_build_ids)},); runOnlyForDeploymentPostprocessing = 0;')
project_configs, target_configs = [], []
for mode in ["Debug", "Release"]:
    project_configs.append(obj("project:" + mode, f'isa = XCBuildConfiguration; name = {mode}; buildSettings = {{ CLANG_ENABLE_MODULES = YES; SWIFT_VERSION = 5.0; ONLY_ACTIVE_ARCH = {"YES" if mode == "Debug" else "NO"}; SWIFT_OPTIMIZATION_LEVEL = "{"-Onone" if mode == "Debug" else "-O"}"; }};'))
    target_configs.append(obj("target:" + mode, f'''isa = XCBuildConfiguration; name = {mode}; buildSettings = {{
        CONTROL_PLANE_BUNDLE_IDENTIFIER = dev.controlplane.app;
        PRODUCT_BUNDLE_IDENTIFIER = "$(CONTROL_PLANE_BUNDLE_IDENTIFIER)";
        PRODUCT_NAME = ControlPlane;
        SWIFT_MODULE_NAME = ControlPlane;
        ENABLE_TESTABILITY = {"YES" if mode == "Debug" else "NO"};
        ASSETCATALOG_COMPILER_APPICON_NAME = AppIcon;
        INFOPLIST_FILE = Info.plist;
        GENERATE_INFOPLIST_FILE = NO;
        CODE_SIGN_STYLE = Automatic;
        CURRENT_PROJECT_VERSION = 1;
        MARKETING_VERSION = 0.1.0;
        MACOSX_DEPLOYMENT_TARGET = 15.0;
        IPHONEOS_DEPLOYMENT_TARGET = 18.0;
        SUPPORTED_PLATFORMS = "macosx iphoneos iphonesimulator";
        SDKROOT = auto;
        TARGETED_DEVICE_FAMILY = "1,2";
        ENABLE_APP_SANDBOX = NO;
        ENABLE_HARDENED_RUNTIME = YES;
        "CODE_SIGN_ENTITLEMENTS[sdk=macosx*]" = ControlPlane.entitlements;
        "CODE_SIGN_ENTITLEMENTS[sdk=iphoneos*]" = ControlPlane-iOS.entitlements;
        CONTROL_PLANE_APNS_ENVIRONMENT = {"sandbox" if mode == "Debug" else "production"};
        CONTROL_PLANE_APS_ENTITLEMENT = {"development" if mode == "Debug" else "production"};
        SWIFT_EMIT_LOC_STRINGS = YES;
    }};'''))
pc = obj("project-configs", f'isa = XCConfigurationList; buildConfigurations = ({",".join(project_configs)},); defaultConfigurationIsVisible = 0; defaultConfigurationName = Release;')
tc = obj("target-configs", f'isa = XCConfigurationList; buildConfigurations = ({",".join(target_configs)},); defaultConfigurationIsVisible = 0; defaultConfigurationName = Release;')
target = obj("target", f'isa = PBXNativeTarget; name = ControlPlane; productName = ControlPlane; productType = "com.apple.product-type.application"; productReference = {app}; buildConfigurationList = {tc}; buildPhases = ({phase},{frameworks},{resources},); buildRules = (); dependencies = (); packageProductDependencies = ({",".join(package_products)},);')
test_targets=[];test_groups=[];test_products=[]
for name,folder,kind in [("ControlPlaneTests","Tests/ControlPlaneKitTests","unit-test"),("ControlPlaneUITests","UITests","ui-testing")]:
 refs=[];builds=[];resource_ids=[]
 for path in sorted((root/folder).glob("*.swift")):
  relative=str(path.relative_to(root))
  ref=obj(relative,f'isa = PBXFileReference; lastKnownFileType = sourcecode.swift; path = "{relative}"; sourceTree = "<group>";')
  refs.append(ref);builds.append(obj("build:"+relative,f'isa = PBXBuildFile; fileRef = {ref};'))
 if kind=="unit-test":
  relative="Tests/ControlPlaneKitTests/Fixtures/synthetic-h264.cpv"
  ref=obj(relative,f'isa = PBXFileReference; lastKnownFileType = file; path = "{relative}"; sourceTree = "<group>";')
  refs.append(ref);resource_ids.append(obj("resource:"+relative,f'isa = PBXBuildFile; fileRef = {ref};'))
 test_groups.append(obj(name+":group",f'isa = PBXGroup; name = {name}; children = ({",".join(refs)},); sourceTree = "<group>";'))
 product=obj(name+":product",f'isa = PBXFileReference; explicitFileType = wrapper.cfbundle; path = {name}.xctest; sourceTree = BUILT_PRODUCTS_DIR;')
 test_products.append(product)
 src=obj(name+":sources",f'isa = PBXSourcesBuildPhase; buildActionMask = 2147483647; files = ({",".join(builds)},); runOnlyForDeploymentPostprocessing = 0;')
 res=obj(name+":resources",f'isa = PBXResourcesBuildPhase; buildActionMask = 2147483647; files = ({",".join(resource_ids)}); runOnlyForDeploymentPostprocessing = 0;')
 test_frameworks=[]
 fw=obj(name+":frameworks",f'isa = PBXFrameworksBuildPhase; buildActionMask = 2147483647; files = ({",".join(test_frameworks)}); runOnlyForDeploymentPostprocessing = 0;')
 configurations=[]
 for mode in ["Debug","Release"]:
  host='TEST_HOST = "$(BUILT_PRODUCTS_DIR)/ControlPlane.app/ControlPlane"; BUNDLE_LOADER = "$(TEST_HOST)";' if kind=="unit-test" else ''
  configurations.append(obj(name+":"+mode,f'''isa = XCBuildConfiguration; name = {mode}; buildSettings = {{
   CONTROL_PLANE_BUNDLE_IDENTIFIER = dev.controlplane.app;
   PRODUCT_BUNDLE_IDENTIFIER = "$(CONTROL_PLANE_BUNDLE_IDENTIFIER).{name}";
   PRODUCT_NAME = {name}; GENERATE_INFOPLIST_FILE = YES; CODE_SIGN_STYLE = Automatic;
   SDKROOT = iphoneos; SUPPORTED_PLATFORMS = "iphoneos iphonesimulator";
   IPHONEOS_DEPLOYMENT_TARGET = 18.0; TARGETED_DEVICE_FAMILY = "1,2";
   SWIFT_VERSION = 5.0; SWIFT_ACTIVE_COMPILATION_CONDITIONS = "$(inherited) CONTROL_PLANE_XCODE_TESTS";
   ENABLE_TESTING_SEARCH_PATHS = YES; TEST_TARGET_NAME = ControlPlane; {host}
  }};'''))
 configs=obj(name+":configs",f'isa = XCConfigurationList; buildConfigurations = ({",".join(configurations)},); defaultConfigurationIsVisible = 0; defaultConfigurationName = Debug;')
 dependency=obj(name+":dependency",f'isa = PBXTargetDependency; target = {target};')
 test_targets.append(obj(name,f'isa = PBXNativeTarget; name = {name}; productName = {name}; productType = "com.apple.product-type.bundle.{kind}"; productReference = {product}; buildConfigurationList = {configs}; buildPhases = ({src},{fw},{res},); dependencies = ({dependency},); packageProductDependencies = ();'))
obj("products",f'isa = PBXGroup; name = Products; children = ({",".join([app]+test_products)},); sourceTree = "<group>";')
obj("group",f'isa = PBXGroup; children = ({",".join(file_ids+test_groups+[products])},); sourceTree = "<group>";')
pid = obj("project", f'isa = PBXProject; attributes = {{ LastUpgradeCheck = 1600; }}; buildConfigurationList = {pc}; compatibilityVersion = "Xcode 14.0"; developmentRegion = en; knownRegions = (en,Base,); mainGroup = {group}; productRefGroup = {products}; projectDirPath = ""; projectRoot = ""; targets = ({",".join([target]+test_targets)},); packageReferences = ({",".join(package_ids)},);')
(project / "project.pbxproj").write_text("// !$*UTF8*$!\n{ archiveVersion = 1; classes = {}; objectVersion = 56; objects = {\n" + "\n".join(line.rstrip() for obj in objects for line in obj.splitlines()) + f"\n}}; rootObject = {pid}; }}\n")
print(project)
schemes=project/"xcshareddata/xcschemes";schemes.mkdir(parents=True,exist_ok=True)
def reference(target_id,name,product):
 return f'<BuildableReference BuildableIdentifier="primary" BlueprintIdentifier="{target_id}" BuildableName="{product}" BlueprintName="{name}" ReferencedContainer="container:ControlPlane.xcodeproj"/>'
app_ref=reference(target,"ControlPlane","ControlPlane.app")
for scheme,include_tests in [("ControlPlane",False),("ControlPlane-iOSTests",True)]:
 build=f'<BuildActionEntry buildForTesting="YES" buildForRunning="YES" buildForProfiling="YES" buildForArchiving="YES" buildForAnalyzing="YES">{app_ref}</BuildActionEntry>'
 testables=''
 if include_tests:
  for target_id,name in zip(test_targets,["ControlPlaneTests","ControlPlaneUITests"]):
   ref=reference(target_id,name,name+".xctest")
   build+=f'<BuildActionEntry buildForTesting="YES" buildForRunning="NO" buildForProfiling="NO" buildForArchiving="NO" buildForAnalyzing="YES">{ref}</BuildActionEntry>'
   testables+=f'<TestableReference skipped="NO">{ref}</TestableReference>'
 (schemes/(scheme+".xcscheme")).write_text(f'''<?xml version="1.0" encoding="UTF-8"?>
<Scheme LastUpgradeVersion="1600" version="1.3">
 <BuildAction parallelizeBuildables="YES" buildImplicitDependencies="YES"><BuildActionEntries>{build}</BuildActionEntries></BuildAction>
 <TestAction buildConfiguration="Debug" selectedDebuggerIdentifier="Xcode.DebuggerFoundation.Debugger.LLDB" selectedLauncherIdentifier="Xcode.IDEFoundation.Launcher.LLDB" shouldUseLaunchSchemeArgsEnv="YES"><Testables>{testables}</Testables><MacroExpansion>{app_ref}</MacroExpansion></TestAction>
 <LaunchAction buildConfiguration="Debug" selectedDebuggerIdentifier="Xcode.DebuggerFoundation.Debugger.LLDB" selectedLauncherIdentifier="Xcode.IDEFoundation.Launcher.LLDB" launchStyle="0" useCustomWorkingDirectory="NO" ignoresPersistentStateOnLaunch="NO" debugDocumentVersioning="YES" debugServiceExtension="internal" allowLocationSimulation="YES"><BuildableProductRunnable runnableDebuggingMode="0">{app_ref}</BuildableProductRunnable></LaunchAction>
 <ProfileAction buildConfiguration="Release" shouldUseLaunchSchemeArgsEnv="YES" useCustomWorkingDirectory="NO" debugDocumentVersioning="YES"><BuildableProductRunnable runnableDebuggingMode="0">{app_ref}</BuildableProductRunnable></ProfileAction>
 <AnalyzeAction buildConfiguration="Debug"/><ArchiveAction buildConfiguration="Release" revealArchiveInOrganizer="YES"/>
</Scheme>
''')
