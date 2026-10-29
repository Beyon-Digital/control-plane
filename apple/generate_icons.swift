// Original vector app mark, rasterized deterministically for Apple asset catalogs.
import Foundation
import CoreGraphics
import ImageIO
let output=URL(fileURLWithPath:CommandLine.arguments[1],isDirectory:true).appendingPathComponent("AppIcon.appiconset",isDirectory:true)
try FileManager.default.createDirectory(at:output,withIntermediateDirectories:true)
func render(_ pixels:Int,mac:Bool)throws{
 let context=CGContext(data:nil,width:pixels,height:pixels,bitsPerComponent:8,bytesPerRow:pixels*4,space:CGColorSpaceCreateDeviceRGB(),bitmapInfo:(mac ? CGImageAlphaInfo.premultipliedLast:CGImageAlphaInfo.noneSkipLast).rawValue)!
 context.scaleBy(x:CGFloat(pixels),y:CGFloat(pixels))
 if mac{context.addPath(CGPath(roundedRect:CGRect(x:0.065,y:0.065,width:0.87,height:0.87),cornerWidth:0.19,cornerHeight:0.19,transform:nil));context.clip()}
 let colors=[CGColor(red:0.18,green:0.18,blue:0.46,alpha:1),CGColor(red:0.31,green:0.31,blue:0.78,alpha:1)]
 let gradient=CGGradient(colorsSpace:CGColorSpaceCreateDeviceRGB(),colors:colors as CFArray,locations:[0,1])!
 context.drawLinearGradient(gradient,start:CGPoint(x:0,y:0),end:CGPoint(x:1,y:1),options:[.drawsBeforeStartLocation,.drawsAfterEndLocation])
 context.setStrokeColor(CGColor(red:0.39,green:0.91,blue:0.86,alpha:0.8));context.setLineWidth(0.018)
 for y in [0.32,0.5,0.68]{context.move(to:CGPoint(x:0.62,y:0.5));context.addLine(to:CGPoint(x:0.79,y:y));context.strokePath()}
 let terminal=CGPath(roundedRect:CGRect(x:0.18,y:0.28,width:0.55,height:0.44),cornerWidth:0.07,cornerHeight:0.07,transform:nil)
 context.addPath(terminal);context.setFillColor(CGColor(red:0.06,green:0.08,blue:0.19,alpha:1));context.fillPath()
 context.addPath(terminal);context.setStrokeColor(CGColor(gray:1,alpha:0.25));context.setLineWidth(0.012);context.strokePath()
 context.setStrokeColor(CGColor(gray:1,alpha:1));context.setLineWidth(0.035);context.setLineCap(.round);context.setLineJoin(.round)
 context.move(to:CGPoint(x:0.29,y:0.59));context.addLine(to:CGPoint(x:0.39,y:0.5));context.addLine(to:CGPoint(x:0.29,y:0.41));context.strokePath()
 context.move(to:CGPoint(x:0.48,y:0.40));context.addLine(to:CGPoint(x:0.61,y:0.40));context.strokePath()
 context.setFillColor(CGColor(red:0.55,green:0.98,blue:0.88,alpha:1))
 for y in [0.32,0.5,0.68]{context.fillEllipse(in:CGRect(x:0.755,y:y-0.035,width:0.07,height:0.07))}
 let image=context.makeImage()!;let url=output.appendingPathComponent("\(mac ? "mac":"ios")-\(pixels).png")
 let destination=CGImageDestinationCreateWithURL(url as CFURL,"public.png" as CFString,1,nil)!
 CGImageDestinationAddImage(destination,image,nil);guard CGImageDestinationFinalize(destination)else{throw NSError(domain:"AppIcon",code:1)}
}
var images:[[String:String]]=[]
var rendered=Set<Int>()
for size in [16,32,128,256,512]{for scale in [1,2]{let pixels=size*scale;if rendered.insert(pixels).inserted{try render(pixels,mac:true)};images.append(["idiom":"mac","size":"\(size)x\(size)","scale":"\(scale)x","filename":"mac-\(pixels).png"])}}
try render(1024,mac:false)
images.append(["idiom":"universal","platform":"ios","size":"1024x1024","filename":"ios-1024.png"])
let data=try JSONSerialization.data(withJSONObject:["images":images,"info":["author":"xcode","version":1]],options:[.prettyPrinted,.sortedKeys])
try data.write(to:output.appendingPathComponent("Contents.json"))
