<?xml version="1.0" encoding="ISO-8859-1"?>

<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform">


<xsl:template match="/">

  <html>
  <head>
  <title>Description Messages</title>
  <style type="text/css">
	hr {
		color:sienna;
		size=1;
	}
	p {
		margin-left:20px;
	}
	body {
		color=black;
		background-color=white;
		font-family:verdana;
		font-size:12px;
		
	}
	
	th{
		font-size:12px;
	}
	
	td{
		font-size:12px;
	}
	
	td.topalign{
		vertical-align:top;
		align:left;
	}
	
	td.midalign{
		vertical-align:middle;
	}
	
	td div.summary{
		font-weight:bold;
	}
	
	ul.logsummarylist{
		margin-top:5px;
		margin-bottom:1px;
		padding:-5px;
		
		
	}
	
	div.redlabel{
		color:red;
		font-weight:bold;
	}
	
	div.blacklabel{
		color=black;
	}
	
	div.greenlabel{
		color:green;
		font-weight:bold;
	}
	
	.notice{
		text-align:right;
		font-size:10px;
	}
	
 </style>
 
  </head>
  <body>
  <h2>Logger</h2>
  <div class="version">Log of <xsl:value-of select="ERRDESCR/HEADER/TOOL"/> [Version: <xsl:value-of select="ERRDESCR/HEADER/VERSION"/>]</div>

  <xsl:variable name="header"> 
  <tr bgcolor="#9acd32">
			 <th width="7%">Where</th>
			 <th width="4%">ErrorCode</th>
			 <th width="10%" colspan="2">Position<br/>row|col</th>
			 <th width="2%">Type</th>
			 <th width="2%">P/F</th>
			 <th width="40%">Description</th>
			 <th width="40%">Suggestion</th>
		</tr>
  </xsl:variable>
    
  <xsl:choose>
	 <xsl:when test="ERRDESCR/SUMMARY/ERRORS + ERRDESCR/SUMMARY/ABORTS + ERRDESCR/SUMMARY/UNKNOWN = 0 ">

	   <div class="logsummaryempty">
			<hr/>
			
			<div class="greenlabel"><img src="http://padwczc13253rp.eu.infineon.com/misc/green.bmp" title="PASS!" /> PASS </div>
			
			</div>
	</xsl:when>
		
	<xsl:otherwise>
	  <div class="logsummary">
		<hr/>
		<div class="redlabel"><img src="http://padwczc13253rp.eu.infineon.com/misc/red.bmp" title="FAIL!" /> FAIL </div>
	  </div>
	</xsl:otherwise>
</xsl:choose>

  <p>
   <ul class="logsummarylist">
 
	<xsl:if test="ERRDESCR/SUMMARY/ABORTS &gt; 0">
      <li>aborts : <strong><xsl:value-of select="ERRDESCR/SUMMARY/ABORTS"/></strong></li>
     
   </xsl:if>
   
   <xsl:if test="ERRDESCR/SUMMARY/ERRORS &gt; 0">
      <li>errors : <strong><xsl:value-of select="ERRDESCR/SUMMARY/ERRORS"/></strong></li>
      
   </xsl:if>
   
   <xsl:if test="ERRDESCR/SUMMARY/INFOS &gt; 0">
      <li>infos : <strong><xsl:value-of select="ERRDESCR/SUMMARY/INFOS"/></strong></li>
     
   </xsl:if>

	<xsl:if test="ERRDESCR/SUMMARY/WARNINGS &gt; 0">
      <li>warnings : <strong><xsl:value-of select="ERRDESCR/SUMMARY/WARNINGS"/></strong></li>
      
   </xsl:if>
   <xsl:if test="ERRDESCR/SUMMARY/UNKNOWN &gt; 0">
      <li>unknowns : <strong><xsl:value-of select="ERRDESCR/SUMMARY/UNKNOWN"/></strong></li>
     
   </xsl:if>
      
  </ul>
  </p>


 <xsl:choose>
	<xsl:when test="ERRDESCR/SUMMARY/ERRORS + ERRDESCR/SUMMARY/ABORTS + ERRDESCR/SUMMARY/UNKNOWN + ERRDESCR/SUMMARY/INFOS + ERRDESCR/SUMMARY/WARNINGS = 0 ">
		
		<p>
			<ul>
				<li>No Aborts found!</li>
				<li>No Errors found!</li>
				<li>No Chash  found!</li>
			</ul>
		</p>
</xsl:when>

		
<xsl:otherwise>
  
<div class="loglist">
  <div class="notice">Results are ordered by type and position/row .</div>
  <hr/>
  
  <table border="1">
  	<xsl:copy-of select="$header" />
 
		
    <xsl:for-each select="ERRDESCR/CHECKER/C">
    <xsl:sort select="TYPE"/>
    <xsl:sort select="POS/ROW" data-type="number"/>
				<xsl:choose>
					<xsl:when test="TYPE !='I'">
    <tr>
	  <td class="midalign">
      	<xsl:value-of select="WHERE"/><br/><xsl:value-of select="WHAT"/>
      </td>
      <td class="midalign">
      	<xsl:value-of select="Code"/> - <xsl:value-of select="SubCode"/>
      </td>
		  <td> 
	  		<xsl:value-of select="POS/ROW"/>
		  </td>
	  <td nowrap="nowrap">
	  
	  <xsl:choose>
         <xsl:when test="POS/COL &gt; -1">
           <xsl:value-of select="POS/COL"/>
        </xsl:when>

         <xsl:otherwise>
           ----
        </xsl:otherwise>
       </xsl:choose>

	  </td>
	  <td><xsl:value-of select="TYPE"/></td>
	  <td>
	  	
	  	
	  	 <xsl:choose>
         <xsl:when test="CLASS/TYPE = 'Fail'">
           <div class="redlabel">
          		<xsl:value-of select="CLASS/TYPE"/>
        	</div> 		
        </xsl:when>

         <xsl:otherwise>
          <div class="blacklabel">
          	<xsl:value-of select="CLASS/TYPE"/>
        	</div>
        </xsl:otherwise>
       </xsl:choose>
					
	  	
	  </td>
	  <td><xsl:value-of select="DESCR"/></td>
	  <td>
	  
	  	<xsl:value-of select="EXAMPLE"/> .<br/>
	  	

	  </td>
    </tr>
					</xsl:when>
				</xsl:choose>
	
    </xsl:for-each>
  </table>
  </div>

  
<div class="loglist">
  <hr/>
  <h3> Info Messages</h3>
  
  <table border="1">
  	<xsl:copy-of select="$header" />
 
		
    <xsl:for-each select="ERRDESCR/CHECKER/C">
    <xsl:sort select="TYPE"/>
    <xsl:sort select="POS/ROW" data-type="number"/>
				<xsl:choose>
					<xsl:when test="TYPE ='I'">
    <tr>
	  <td class="midalign">
      	<xsl:value-of select="WHERE"/><br/><xsl:value-of select="WHAT"/>
      </td>
      <td class="midalign">
      	<xsl:value-of select="Code"/> - <xsl:value-of select="SubCode"/>
      </td>
		  <td> 
	  		<xsl:value-of select="POS/ROW"/>
		  </td>
	  <td nowrap="nowrap">
	  
	  <xsl:choose>
         <xsl:when test="POS/COL &gt; -1">
           <xsl:value-of select="POS/COL"/>
        </xsl:when>

         <xsl:otherwise>
           ----
        </xsl:otherwise>
       </xsl:choose>

	  </td>
	  <td><xsl:value-of select="TYPE"/></td>
	  <td>
	  	
	  	
	  	 <xsl:choose>
         <xsl:when test="CLASS/TYPE = 'Fail'">
           <div class="redlabel">
          		<xsl:value-of select="CLASS/TYPE"/>
        	</div> 		
        </xsl:when>

         <xsl:otherwise>
          <div class="blacklabel">
          	<xsl:value-of select="CLASS/TYPE"/>
        	</div>
        </xsl:otherwise>
       </xsl:choose>
					
	  	
	  </td>
	  <td><xsl:value-of select="DESCR"/></td>
	  <td>
	  
	  	<xsl:value-of select="EXAMPLE"/> .<br/>
	  	

	  </td>
    </tr>
					</xsl:when>
				</xsl:choose>
	
    </xsl:for-each>
  </table>
  </div>

  
  </xsl:otherwise>
</xsl:choose>
  </body>
  </html>
</xsl:template>

</xsl:stylesheet> 

